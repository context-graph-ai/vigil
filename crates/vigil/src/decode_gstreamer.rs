//! GStreamer-backed hardware decode behind the [`DecodeBackend`] seam.
//!
//! GStreamer owns plugin discovery and decoder auto-plugging (appsrc →
//! parser → decodebin3 → videoconvert → bounded appsink); Vigil owns the
//! decoder trait, scheduler-ready output, and receipt semantics. Active
//! hardware decode requires BOTH a passed real decode probe AND a selected
//! decoder element explicitly classified as hardware — GStreamer
//! initialization, plugin presence, or an unclassifiable selection never
//! claims hardware.
//!
//! Compiled only behind the `decode-gstreamer` cargo feature: the default
//! static-musl artifact never includes this module and reports
//! `unsupported_by_this_artifact` instead.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;

use crate::acceleration::{ActionKind, EvidenceKind, FailureCode};
use crate::decode::{
    BackendClassification, DecodeBackend, DecodeBackendError, DecoderSelection, EncodedAccessUnit,
    ProbeOutcome,
};
use crate::media_pipeline::{DecodedRgbFrame, VideoCodec};
use crate::workgraph::StreamId;

/// Why the GStreamer path was not used, in receipt vocabulary. The caller
/// (decode selection / doctor) folds this into the fallback receipt.
pub struct FallbackFinding {
    pub failure_code: FailureCode,
    pub evidence_kind: EvidenceKind,
    pub evidence_fields: BTreeMap<String, String>,
    pub action_kind: ActionKind,
    pub action_payload: Option<String>,
}

impl FallbackFinding {
    fn new(failure_code: FailureCode, evidence_kind: EvidenceKind) -> Self {
        Self {
            failure_code,
            evidence_kind,
            evidence_fields: BTreeMap::new(),
            action_kind: ActionKind::ManualActionRequired,
            action_payload: None,
        }
    }
}

/// How the bounded probe drain (`GstreamerDecodeBackend::drain_probe`)
/// finished: a genuine decoder failure (end-of-stream with nothing
/// produced) is a DISTINCT outcome from a probe window that simply wasn't
/// long enough for a slow cold start — a timeout must never be reported the
/// same way as "hardware decode unavailable".
enum ProbeDrainOutcome {
    Decoded,
    EndOfStreamNoFrames,
    TimedOut,
}

/// The effective probe-drain deadline. The default is long enough for a
/// cold hardware decoder's first-frame latency (VA-API/NVDEC init inside a
/// container, in particular — the documented failure this bound exists to
/// fix) without hanging startup indefinitely; a box whose decoder needs
/// even longer can raise it via `VIGIL_DECODE_PROBE_DEADLINE_SECS`. An
/// unparseable or non-positive value falls back to the default.
fn decode_probe_deadline() -> Duration {
    const DEFAULT: Duration = Duration::from_secs(5);
    std::env::var("VIGIL_DECODE_PROBE_DEADLINE_SECS")
        .ok()
        .and_then(|raw| raw.trim().parse::<f64>().ok())
        .filter(|secs| secs.is_finite() && *secs > 0.0)
        .map(Duration::from_secs_f64)
        .unwrap_or(DEFAULT)
}

/// Decoder factories that are known CPU/software implementations. Anything
/// not klass-tagged "Hardware" and not in this set stays Unclassified — and
/// Unclassified never claims hardware.
const KNOWN_SOFTWARE_DECODERS: &[&str] = &[
    "openh264dec",
    "avdec_h264",
    "avdec_h265",
    "avdec_hevc",
    "libde265dec",
    "vp8dec",
    "vp9dec",
];

pub struct GstreamerDecodeBackend {
    stream_id: StreamId,
    stream_epoch: u64,
    codec: VideoCodec,
    pipeline: gst::Pipeline,
    appsrc: gst_app::AppSrc,
    appsink: gst_app::AppSink,
    classification: BackendClassification,
    next_frame_index: u64,
}

impl GstreamerDecodeBackend {
    /// Build the pipeline, run the REAL decode probe on the sample units,
    /// classify the selected decoder, and return a working hardware-backed
    /// selection — or a receipt-shaped reason to fall back.
    pub fn probe_and_build(
        stream_id: StreamId,
        codec: VideoCodec,
        stream_epoch: u64,
        probe_sample: &[EncodedAccessUnit],
    ) -> Result<DecoderSelection, Box<FallbackFinding>> {
        if let Err(error) = gst::init() {
            let mut finding = FallbackFinding::new(
                FailureCode::MissingRuntimeDependency,
                EvidenceKind::RuntimeDependency,
            );
            finding
                .evidence_fields
                .insert("dependency".to_string(), "gstreamer-core".to_string());
            finding
                .evidence_fields
                .insert("error".to_string(), error.to_string());
            finding.action_kind = ActionKind::InstallPackageProfile;
            finding.action_payload =
                Some("install the GStreamer runtime for this platform".to_string());
            return Err(Box::new(finding));
        }

        if probe_sample.is_empty() {
            // Distinguish "the probe INPUT could not be produced" from a
            // decoder that failed: blaming hardware for a missing sample
            // sends the operator chasing the wrong problem.
            let mut finding =
                FallbackFinding::new(FailureCode::ProbeFailed, EvidenceKind::UpstreamError);
            finding.evidence_fields.insert(
                "error".to_string(),
                "no probe sample units were available (probe input generation failed)".to_string(),
            );
            return Err(Box::new(finding));
        }
        // The probe pipeline is disposable: it exists only to push the
        // finite probe sample, flush it with EOS, and let the classifier
        // read whichever decoder decodebin3 auto-plugged. It is torn down
        // below either way; live capture always gets a FRESH pipeline built
        // right before use, never this consumed one.
        let mut probe_backend = match Self::build_pipeline(stream_id.clone(), codec, stream_epoch) {
            Ok(backend) => backend,
            Err(finding) => return Err(Box::new(finding)),
        };

        // Push the whole finite probe sample directly — bypassing
        // `decode()`'s per-unit live-capture drain, which exists for the
        // low-latency capture loop, not a bounded startup probe — then end
        // the stream so a cold-start hardware decoder still flushes its
        // pending output instead of racing a short first-frame window.
        for unit in probe_sample {
            if let Err(error) = probe_backend.check_unit(unit) {
                probe_backend.teardown();
                let mut finding =
                    FallbackFinding::new(FailureCode::ProbeFailed, EvidenceKind::BackendProbe);
                finding
                    .evidence_fields
                    .insert("error".to_string(), format!("{error:?}"));
                return Err(Box::new(finding));
            }
            let buffer = gst::Buffer::from_slice(unit.data.clone());
            if let Err(error) = probe_backend.appsrc.push_buffer(buffer) {
                probe_backend.teardown();
                let mut finding =
                    FallbackFinding::new(FailureCode::ProbeFailed, EvidenceKind::BackendProbe);
                finding
                    .evidence_fields
                    .insert("error".to_string(), format!("appsrc push: {error}"));
                return Err(Box::new(finding));
            }
        }
        let _ = probe_backend.appsrc.end_of_stream();

        let deadline = decode_probe_deadline();
        let (frames_decoded, outcome) = match probe_backend.drain_probe(deadline) {
            Ok(result) => result,
            Err(error) => {
                probe_backend.teardown();
                let mut finding =
                    FallbackFinding::new(FailureCode::ProbeFailed, EvidenceKind::BackendProbe);
                finding
                    .evidence_fields
                    .insert("error".to_string(), format!("{error:?}"));
                return Err(Box::new(finding));
            }
        };
        match outcome {
            ProbeDrainOutcome::TimedOut => {
                probe_backend.teardown();
                let mut finding =
                    FallbackFinding::new(FailureCode::ProbeFailed, EvidenceKind::BackendProbe);
                finding.evidence_fields.insert(
                    "error".to_string(),
                    format!(
                        "no decoded frame within {:.1}s probe window; a slow decoder cold \
                         start can exceed it - raise VIGIL_DECODE_PROBE_DEADLINE_SECS and \
                         restart to retry",
                        deadline.as_secs_f64()
                    ),
                );
                return Err(Box::new(finding));
            }
            ProbeDrainOutcome::EndOfStreamNoFrames => {
                probe_backend.teardown();
                let mut finding =
                    FallbackFinding::new(FailureCode::ProbeFailed, EvidenceKind::BackendProbe);
                finding.evidence_fields.insert(
                    "error".to_string(),
                    "decoder reached end of stream without producing a frame".to_string(),
                );
                return Err(Box::new(finding));
            }
            ProbeDrainOutcome::Decoded => {}
        }
        debug_assert!(frames_decoded > 0);

        // Classify the decoder decodebin3 actually selected — read from the
        // PROBE pipeline before it is torn down.
        let classification = classify_selected_decoder(&probe_backend.pipeline);
        probe_backend.teardown();

        match &classification {
            BackendClassification::Hardware { element, device } => {
                let receipt = crate::acceleration::AccelerationReceipt {
                    stage: crate::acceleration::AccelStage::Decode,
                    work_id: None,
                    parent_work_id: None,
                    stream_id: Some(stream_id.clone()),
                    media_item: None,
                    configured: true,
                    attempted_backend: "gstreamer".to_string(),
                    active_backend: "gstreamer".to_string(),
                    hardware_accelerated: true,
                    selected_device: device.clone(),
                    codec: Some(
                        match codec {
                            VideoCodec::H264 => "H264",
                            VideoCodec::H265 => "H265",
                        }
                        .to_string(),
                    ),
                    model_id: None,
                    model_version: None,
                    input_shape: None,
                    probe_status: crate::acceleration::ProbeStatus::Active,
                    failure_code: FailureCode::None,
                    evidence_kind: Some(EvidenceKind::SelectedBackend),
                    evidence_fields: BTreeMap::from([
                        ("selected_decoder".to_string(), element.clone()),
                        // Startup-probe cost: the probe ran on these real
                        // stream units; the capture loop replays them, so
                        // the first segment still starts at the stream's
                        // first parameter-set boundary.
                        (
                            "probe_units_consumed".to_string(),
                            probe_sample.len().to_string(),
                        ),
                        (
                            "probe_frames_decoded".to_string(),
                            frames_decoded.to_string(),
                        ),
                    ]),
                    action_kind: ActionKind::NoAction,
                    action_payload: None,
                };
                // Live capture gets a FRESH pipeline, never the probe's
                // consumed one (its appsrc already reached end-of-stream and
                // cannot accept more buffers): the receipt's classification
                // and the fresh backend's classification are the SAME
                // observed fact, just applied to two different pipeline
                // instances of the identical decoder selection.
                let mut live_backend = match Self::build_pipeline(stream_id, codec, stream_epoch) {
                    Ok(backend) => backend,
                    Err(finding) => return Err(Box::new(finding)),
                };
                live_backend.classification = classification.clone();
                Ok(DecoderSelection {
                    backend: Box::new(live_backend),
                    receipt,
                })
            }
            BackendClassification::Software { element } => {
                let element = element.clone();
                let mut finding = FallbackFinding::new(
                    FailureCode::MissingRuntimeDependency,
                    EvidenceKind::SelectedBackend,
                );
                finding
                    .evidence_fields
                    .insert("selected_decoder".to_string(), element);
                let (action_kind, action_payload) =
                    plugin_install_action("a hardware video decoder (VA plugin family)");
                finding.action_kind = action_kind;
                finding.action_payload = action_payload;
                Err(Box::new(finding))
            }
            BackendClassification::Unclassified { element } => {
                let element = element.clone();
                let mut finding = FallbackFinding::new(
                    FailureCode::UnclassifiedSelectedDecoder,
                    EvidenceKind::SelectedBackend,
                );
                finding
                    .evidence_fields
                    .insert("selected_decoder".to_string(), element);
                finding.action_payload = Some(
                    "the selected decoder could not be classified as hardware; \
                     refusing to claim hardware acceleration"
                        .to_string(),
                );
                Err(Box::new(finding))
            }
        }
    }

    /// Drain the disposable probe pipeline under a bounded wall-clock
    /// deadline (never the live capture loop's 10ms budget — see
    /// `drain_available`). Loops `try_pull_sample` with a short per-pull
    /// budget, accumulating every frame the finite probe produces, until:
    /// at least one frame has arrived and the sink has nothing more ready
    /// right now (`Decoded`); the sink reaches end-of-stream with zero
    /// frames, a genuine decode failure (`EndOfStreamNoFrames`); or the
    /// deadline elapses with zero frames, a cold-start-too-slow timeout
    /// that must never be reported as hardware-unavailable (`TimedOut`).
    fn drain_probe(
        &mut self,
        deadline: Duration,
    ) -> Result<(u64, ProbeDrainOutcome), DecodeBackendError> {
        let started = Instant::now();
        let per_pull_budget = gst::ClockTime::from_mseconds(100);
        let mut frames_decoded = 0u64;
        loop {
            if let Some(sample) = self.appsink.try_pull_sample(per_pull_budget) {
                self.sample_to_rgb(&sample)?;
                frames_decoded += 1;
                continue;
            }
            // Nothing ready right now: the sink is momentarily drained.
            if frames_decoded > 0 {
                return Ok((frames_decoded, ProbeDrainOutcome::Decoded));
            }
            if self.appsink.is_eos() {
                return Ok((frames_decoded, ProbeDrainOutcome::EndOfStreamNoFrames));
            }
            if started.elapsed() >= deadline {
                return Ok((frames_decoded, ProbeDrainOutcome::TimedOut));
            }
        }
    }

    fn build_pipeline(
        stream_id: StreamId,
        codec: VideoCodec,
        stream_epoch: u64,
    ) -> Result<Self, FallbackFinding> {
        let (caps_name, parser_name) = match codec {
            VideoCodec::H264 => ("video/x-h264", "h264parse"),
            VideoCodec::H265 => ("video/x-h265", "h265parse"),
        };

        let make = |factory: &str| {
            gst::ElementFactory::make(factory).build().map_err(|_| {
                let mut finding = FallbackFinding::new(
                    FailureCode::MissingRuntimeDependency,
                    EvidenceKind::RuntimeDependency,
                );
                finding
                    .evidence_fields
                    .insert("dependency".to_string(), factory.to_string());
                let (action_kind, action_payload) = plugin_install_action(factory);
                finding.action_kind = action_kind;
                finding.action_payload = action_payload;
                finding
            })
        };

        let appsrc = gst_app::AppSrc::builder()
            .caps(
                &gst::Caps::builder(caps_name)
                    .field("stream-format", "byte-stream")
                    .field("alignment", "au")
                    .build(),
            )
            .format(gst::Format::Time)
            .is_live(true)
            .build();
        let parser = make(parser_name)?;
        let decoder = make("decodebin3")?;
        let convert = make("videoconvert")?;
        let appsink = gst_app::AppSink::builder()
            .caps(
                &gst::Caps::builder("video/x-raw")
                    .field("format", "RGB")
                    .build(),
            )
            .max_buffers(8)
            .drop(false)
            .sync(false)
            .build();

        let pipeline = gst::Pipeline::new();
        pipeline
            .add_many([
                appsrc.upcast_ref::<gst::Element>(),
                &parser,
                &decoder,
                convert.upcast_ref(),
                appsink.upcast_ref(),
            ])
            .map_err(|error| {
                let mut finding =
                    FallbackFinding::new(FailureCode::ProbeFailed, EvidenceKind::UpstreamError);
                finding
                    .evidence_fields
                    .insert("error".to_string(), error.to_string());
                finding
            })?;

        gst::Element::link_many([appsrc.upcast_ref::<gst::Element>(), &parser, &decoder]).map_err(
            |error| {
                let mut finding =
                    FallbackFinding::new(FailureCode::ProbeFailed, EvidenceKind::UpstreamError);
                finding
                    .evidence_fields
                    .insert("error".to_string(), error.to_string());
                finding
            },
        )?;
        // decodebin3's video pad appears dynamically; link it to videoconvert
        // when it shows up.
        let convert_clone = convert.clone();
        decoder.connect_pad_added(move |_, pad| {
            if let Some(sink_pad) = convert_clone.static_pad("sink")
                && !sink_pad.is_linked()
            {
                let _ = pad.link(&sink_pad);
            }
        });
        convert
            .link(appsink.upcast_ref::<gst::Element>())
            .map_err(|error| {
                let mut finding =
                    FallbackFinding::new(FailureCode::ProbeFailed, EvidenceKind::UpstreamError);
                finding
                    .evidence_fields
                    .insert("error".to_string(), error.to_string());
                finding
            })?;

        pipeline.set_state(gst::State::Playing).map_err(|error| {
            let mut finding =
                FallbackFinding::new(FailureCode::ProbeFailed, EvidenceKind::UpstreamError);
            finding
                .evidence_fields
                .insert("error".to_string(), error.to_string());
            finding
        })?;

        Ok(Self {
            stream_id,
            stream_epoch,
            codec,
            pipeline,
            appsrc,
            appsink,
            classification: BackendClassification::Unclassified {
                element: "pending".to_string(),
            },
            next_frame_index: 0,
        })
    }

    fn check_unit(&self, unit: &EncodedAccessUnit) -> Result<(), DecodeBackendError> {
        if unit.stream_id != self.stream_id {
            return Err(DecodeBackendError::StreamViolation);
        }
        if unit.stream_epoch != self.stream_epoch {
            return Err(DecodeBackendError::EpochViolation {
                expected: self.stream_epoch,
                got: unit.stream_epoch,
            });
        }
        if unit.codec != self.codec {
            return Err(DecodeBackendError::CodecViolation);
        }
        Ok(())
    }

    /// Pull every decoded frame currently available without blocking the
    /// capture loop.
    fn drain_available(&mut self) -> Result<Vec<DecodedRgbFrame>, DecodeBackendError> {
        let mut frames = Vec::new();
        // One bounded wait for the first frame, then non-blocking pulls:
        // the terminating pull must not tax the capture thread a fixed
        // 20 ms per access unit (that is 60% of real time at 30 fps).
        let mut budget = gst::ClockTime::from_mseconds(10);
        while let Some(sample) = self.appsink.try_pull_sample(budget) {
            frames.push(self.sample_to_rgb(&sample)?);
            budget = gst::ClockTime::ZERO;
        }
        Ok(frames)
    }

    fn sample_to_rgb(
        &mut self,
        sample: &gst::Sample,
    ) -> Result<DecodedRgbFrame, DecodeBackendError> {
        let caps = sample
            .caps()
            .ok_or_else(|| DecodeBackendError::Decode("sample without caps".to_string()))?;
        let info = gst_video::VideoInfo::from_caps(caps)
            .map_err(|error| DecodeBackendError::Decode(format!("caps parse: {error}")))?;
        let buffer = sample
            .buffer()
            .ok_or_else(|| DecodeBackendError::Decode("sample without buffer".to_string()))?;
        let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info)
            .map_err(|_| DecodeBackendError::Decode("frame map failed".to_string()))?;

        let width = info.width();
        let height = info.height();
        let stride = info.stride()[0] as usize;
        let data = frame
            .plane_data(0)
            .map_err(|_| DecodeBackendError::Decode("plane data unavailable".to_string()))?;
        let row_bytes = width as usize * 3;
        // Never trust the plane length: a malformed buffer must be a decode
        // error, not a panic that kills the capture thread.
        let needed = (height as usize)
            .checked_sub(1)
            .and_then(|last| last.checked_mul(stride))
            .and_then(|last_row| last_row.checked_add(row_bytes));
        if needed.is_none_or(|needed| data.len() < needed) {
            return Err(DecodeBackendError::Decode(format!(
                "plane too short: len={} stride={stride} rows={height} row_bytes={row_bytes}",
                data.len()
            )));
        }
        let mut rgb = Vec::with_capacity(row_bytes * height as usize);
        for row in 0..height as usize {
            let start = row * stride;
            rgb.extend_from_slice(&data[start..start + row_bytes]);
        }
        let index = self.next_frame_index;
        self.next_frame_index += 1;
        Ok(DecodedRgbFrame {
            index,
            width,
            height,
            rgb,
        })
    }

    fn teardown(&self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

impl Drop for GstreamerDecodeBackend {
    fn drop(&mut self) {
        self.teardown();
    }
}

impl DecodeBackend for GstreamerDecodeBackend {
    fn id(&self) -> &'static str {
        "gstreamer"
    }

    fn probe(&mut self, sample: &[EncodedAccessUnit]) -> ProbeOutcome {
        let mut frames_decoded = 0u64;
        for unit in sample {
            match self.decode(unit) {
                Ok(frames) => frames_decoded += frames.len() as u64,
                Err(error) => {
                    return ProbeOutcome::Failed {
                        reason: format!("gstreamer decode probe failed: {error:?}"),
                    };
                }
            }
        }
        if frames_decoded == 0 {
            return ProbeOutcome::Failed {
                reason: "gstreamer decode probe produced no frames".to_string(),
            };
        }
        ProbeOutcome::Decoded {
            classification: self.classification(),
            frames_decoded,
        }
    }

    fn decode(
        &mut self,
        unit: &EncodedAccessUnit,
    ) -> Result<Vec<DecodedRgbFrame>, DecodeBackendError> {
        self.check_unit(unit)?;
        let buffer = gst::Buffer::from_slice(unit.data.clone());
        self.appsrc
            .push_buffer(buffer)
            .map_err(|error| DecodeBackendError::Decode(format!("appsrc push: {error}")))?;
        self.drain_available()
    }

    fn classification(&self) -> BackendClassification {
        self.classification.clone()
    }
}

/// Walk the live pipeline and classify the decoder element decodebin3
/// selected. Hardware iff the element factory's klass metadata carries the
/// GStreamer "Hardware" tag; known software factories classify Software;
/// anything else is Unclassified and never claims hardware.
fn classify_selected_decoder(pipeline: &gst::Pipeline) -> BackendClassification {
    let mut selected: Option<(String, String, Option<String>)> = None;
    let mut stack: Vec<gst::Element> = vec![pipeline.clone().upcast()];
    while let Some(element) = stack.pop() {
        if let Some(bin) = element.downcast_ref::<gst::Bin>() {
            for child in bin.children() {
                stack.push(child);
            }
            continue;
        }
        let Some(factory) = element.factory() else {
            continue;
        };
        let klass = factory
            .metadata(gst::ELEMENT_METADATA_KLASS)
            .unwrap_or("")
            .to_string();
        if !(klass.contains("Decoder") && klass.contains("Video")) {
            continue;
        }
        let device = ["device-path", "device"].iter().find_map(|prop| {
            element
                .find_property(prop)
                .filter(|spec| spec.value_type() == gst::glib::Type::STRING)
                .map(|_| element.property::<String>(prop))
                .filter(|value: &String| !value.is_empty())
        });
        selected = Some((factory.name().to_string(), klass, device));
    }

    match selected {
        None => BackendClassification::Unclassified {
            element: "none-selected".to_string(),
        },
        Some((name, klass, device)) => {
            if klass.contains("Hardware") {
                BackendClassification::Hardware {
                    element: name,
                    device,
                }
            } else if KNOWN_SOFTWARE_DECODERS.contains(&name.as_str()) {
                BackendClassification::Software { element: name }
            } else {
                BackendClassification::Unclassified { element: name }
            }
        }
    }
}

/// The paste-ready install action for a missing GStreamer plugin family,
/// derived from /etc/os-release. Package names here are public platform
/// knowledge (the distribution's GStreamer "bad" plugin set, which carries
/// the VA video decoders and H.26x parsers), never deployment-specific.
fn plugin_install_action(missing: &str) -> (ActionKind, Option<String>) {
    let os_release = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    let os = os_release.to_ascii_lowercase();
    let is = |needle: &str| {
        os.lines().any(|line| {
            (line.starts_with("id=") || line.starts_with("id_like=")) && line.contains(needle)
        })
    };
    if is("debian") || is("ubuntu") {
        (
            ActionKind::RunCommand,
            Some(format!(
                "sudo apt install gstreamer1.0-plugins-bad  # provides {missing}"
            )),
        )
    } else if is("alpine") {
        (
            ActionKind::RunCommand,
            Some(format!("apk add gst-plugins-bad  # provides {missing}")),
        )
    } else if is("fedora") || is("rhel") {
        (
            ActionKind::RunCommand,
            Some(format!(
                "sudo dnf install gstreamer1-plugins-bad-free  # provides {missing}"
            )),
        )
    } else {
        (
            ActionKind::InstallPackageProfile,
            Some(format!(
                "install the GStreamer plugin set providing {missing} for this platform"
            )),
        )
    }
}

/// A synthetic H.264 probe sample encoded in-process with OpenH264 — the
/// doctor's real-decode-probe input. No fixture bytes, nothing
/// environment-specific.
pub fn synthetic_h264_probe_sample() -> Vec<EncodedAccessUnit> {
    use openh264::encoder::Encoder;
    use openh264::formats::{RgbSliceU8, YUVBuffer};

    let width = 64usize;
    let height = 64usize;
    let mut units = Vec::new();
    if let Ok(mut encoder) = Encoder::new() {
        for index in 0..8usize {
            let mut rgb = vec![0u8; width * height * 3];
            for (pixel, chunk) in rgb.chunks_exact_mut(3).enumerate() {
                chunk[0] = ((pixel % width) as u8).wrapping_add(index as u8 * 16);
                chunk[1] = (pixel / width) as u8;
                chunk[2] = 96;
            }
            let yuv = YUVBuffer::from_rgb_source(RgbSliceU8::new(&rgb, (width, height)));
            if let Ok(bitstream) = encoder.encode(&yuv) {
                let bytes = bitstream.to_vec();
                if !bytes.is_empty() {
                    units.push(bytes);
                }
            }
        }
    }
    let mut assembler =
        crate::decode::AccessUnitAssembler::new(StreamId::new("doctor-probe"), VideoCodec::H264, 0);
    units
        .into_iter()
        .enumerate()
        .map(|(index, data)| assembler.assemble(data, None, index == 0, 0))
        .collect()
}
