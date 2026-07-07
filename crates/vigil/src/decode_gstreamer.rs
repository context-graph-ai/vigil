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

        let mut backend = match Self::build_pipeline(stream_id, codec, stream_epoch) {
            Ok(backend) => backend,
            Err(finding) => return Err(Box::new(finding)),
        };

        // Real startup decode probe on the actual stream codec path.
        let mut frames_decoded = 0u64;
        for unit in probe_sample {
            match backend.decode(unit) {
                Ok(frames) => frames_decoded += frames.len() as u64,
                Err(error) => {
                    backend.teardown();
                    let mut finding =
                        FallbackFinding::new(FailureCode::ProbeFailed, EvidenceKind::BackendProbe);
                    finding
                        .evidence_fields
                        .insert("error".to_string(), format!("{error:?}"));
                    return Err(Box::new(finding));
                }
            }
        }
        frames_decoded += backend
            .drain_available()
            .map(|frames| frames.len() as u64)
            .unwrap_or(0);
        if frames_decoded == 0 {
            backend.teardown();
            let mut finding =
                FallbackFinding::new(FailureCode::ProbeFailed, EvidenceKind::BackendProbe);
            finding.evidence_fields.insert(
                "error".to_string(),
                "decode probe produced no frames".to_string(),
            );
            return Err(Box::new(finding));
        }

        // Classify the decoder decodebin3 actually selected.
        let classification = classify_selected_decoder(&backend.pipeline);
        backend.classification = classification.clone();
        match &classification {
            BackendClassification::Hardware { element, device } => {
                let receipt = crate::acceleration::AccelerationReceipt {
                    stage: crate::acceleration::AccelStage::Decode,
                    work_id: None,
                    parent_work_id: None,
                    stream_id: Some(backend.stream_id.clone()),
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
                    evidence_fields: BTreeMap::from([(
                        "selected_decoder".to_string(),
                        element.clone(),
                    )]),
                    action_kind: ActionKind::NoAction,
                    action_payload: None,
                };
                Ok(DecoderSelection {
                    backend: Box::new(backend),
                    receipt,
                })
            }
            BackendClassification::Software { element } => {
                let element = element.clone();
                backend.teardown();
                let mut finding = FallbackFinding::new(
                    FailureCode::MissingRuntimeDependency,
                    EvidenceKind::SelectedBackend,
                );
                finding
                    .evidence_fields
                    .insert("selected_decoder".to_string(), element);
                finding.action_kind = ActionKind::InstallPackageProfile;
                finding.action_payload = Some(
                    "GStreamer selected a software decoder: the hardware decoder plugin \
                     for this platform is not installed or not usable"
                        .to_string(),
                );
                Err(Box::new(finding))
            }
            BackendClassification::Unclassified { element } => {
                let element = element.clone();
                backend.teardown();
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
                finding.action_kind = ActionKind::InstallPackageProfile;
                finding.action_payload = Some(format!(
                    "install the GStreamer plugin providing `{factory}`"
                ));
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
        Ok(())
    }

    /// Pull every decoded frame currently available without blocking the
    /// capture loop.
    fn drain_available(&mut self) -> Result<Vec<DecodedRgbFrame>, DecodeBackendError> {
        let mut frames = Vec::new();
        while let Some(sample) = self
            .appsink
            .try_pull_sample(gst::ClockTime::from_mseconds(20))
        {
            frames.push(self.sample_to_rgb(&sample)?);
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
