//! Two related pins on the shared camera encoder seam:
//!
//! - `EncoderConfig` carries the effective per-stream frame rate,
//!   bitrate, and keyframe interval — operator-adjustable levers already
//!   ratified on the settings registry — and the encoder CONSUMES them
//!   directly rather than deriving its own numbers from a resolution
//!   class table (which is what `Openh264SoftwareEncoder::new` still
//!   does, unchanged, for its own pre-existing callers).
//! - Periodic keyframe scheduling is owned by the shared encoder itself
//!   (via `encode_raw`), never by an adapter, and is counted by
//!   SUCCESSFULLY encoded output frames only — never by attempted
//!   inputs, and never by a timer.

use bytes::Bytes;

use vigil::encode::{
    CameraEncoder, EncoderConfig, Openh264SoftwareEncoder, PixelFormat, PlaneLayout, RawFrame,
};

const WIDTH: u32 = 64;
const HEIGHT: u32 = 48;

/// A tightly packed RGB24 `RawFrame` with real, high-entropy structure
/// (a pseudo-random pattern via a simple deterministic LCG, seeded by
/// `index`) — high entropy so a low-bitrate encode is forced to quantize
/// visibly harder than a high-bitrate encode of the SAME content, which
/// is exactly the signal the bitrate-consumption test below depends on.
/// A smooth gradient would compress to a similar size regardless of the
/// configured bitrate and could not distinguish "consumed the config"
/// from "ignored it."
fn noisy_rgb24_frame(width: u32, height: u32, index: u64) -> RawFrame {
    let stride = (width * 3) as usize;
    let mut data = vec![0u8; stride * height as usize];
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15 ^ (index.wrapping_mul(0xD1B5_4A32_D192_ED03));
    for byte in data.iter_mut() {
        // A simple xorshift64* step — deterministic, no time/clock
        // involvement, just a reproducible high-entropy byte stream.
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *byte = (state & 0xFF) as u8;
    }
    RawFrame {
        width,
        height,
        format: PixelFormat::Rgb24,
        planes: vec![PlaneLayout { offset: 0, stride }],
        data: Bytes::from(data),
    }
}

fn config(bitrate_bps: u32, keyframe_interval_frames: u32) -> EncoderConfig {
    EncoderConfig {
        effective_fps: 30.0,
        bitrate_bps,
        keyframe_interval_frames,
    }
}

/// A deterministic, temporally coherent "plasma" RGB24 `RawFrame` at
/// frame `index`: three overlapping sine waves whose phase advances a
/// small, fixed amount per frame. This is spatially smooth (so it is NOT
/// incompressible noise — a real rate controller has real inter-frame
/// redundancy to exploit, unlike `noisy_rgb24_frame` above), yet visibly
/// shifts from one frame to the next (so it IS genuine, non-trivial
/// motion the content stays reachable-but-not-trivial against, unlike a
/// static frame a low bitrate could satisfy regardless of target). No
/// time/clock is read anywhere — `index` is the sole "time" input, so the
/// whole sequence is byte-for-byte reproducible run to run.
fn plasma_rgb24_frame(width: u32, height: u32, index: u64) -> RawFrame {
    let stride = (width * 3) as usize;
    let mut data = vec![0u8; stride * height as usize];
    let phase = index as f64 * 0.35;
    let cx = f64::from(width) / 2.0;
    let cy = f64::from(height) / 2.0;
    for y in 0..height {
        let fy = f64::from(y) / f64::from(height);
        let dy = f64::from(y) - cy;
        for x in 0..width {
            let fx = f64::from(x) / f64::from(width);
            let dx = f64::from(x) - cx;
            // A classic multi-term "plasma" mix: two axis-aligned waves
            // plus a diagonal wave plus a radial ripple centered in the
            // frame — enough real spatial+temporal detail that a higher
            // configured bitrate genuinely has more to spend on, unlike a
            // single low-frequency gradient a low bitrate could already
            // represent losslessly.
            let radius = (dx * dx + dy * dy).sqrt();
            let ripple = (radius * 0.35 - phase * 1.6).sin();
            let r = ((fx * 24.0 + phase).sin() * 0.5 + 0.5) * 255.0;
            let g = ((fy * 19.0 - phase * 1.2).sin() * 0.5 + 0.5) * 255.0;
            let b = ((((fx + fy) * 16.0 + phase * 0.8).sin() + ripple) * 0.25 + 0.5) * 255.0;
            let offset = y as usize * stride + (x * 3) as usize;
            data[offset] = r as u8;
            data[offset + 1] = g as u8;
            data[offset + 2] = b.clamp(0.0, 255.0) as u8;
        }
    }
    RawFrame {
        width,
        height,
        format: PixelFormat::Rgb24,
        planes: vec![PlaneLayout { offset: 0, stride }],
        data: Bytes::from(data),
    }
}

/// Resolution for the long-run bitrate-response measurement below:
/// 640×480 is a realistic camera resolution — it is also this codebase's
/// own documented automatic-bitrate resolution-class boundary (see
/// `BITRATE_BPS_UP_TO_640X480_AUTOMATIC_DEFAULT` in `encode.rs`) — never
/// the tiny 64×48 the scheduling tests above use purely for speed.
const BITRATE_TEST_WIDTH: u32 = 640;
const BITRATE_TEST_HEIGHT: u32 = 480;
/// `effective_fps` for the measurement: a real, modest camera frame rate.
const BITRATE_TEST_EFFECTIVE_FPS: f64 = 15.0;
/// One GOP: at 15fps this is exactly one second of media.
const BITRATE_TEST_GOP_FRAMES: u64 = 15;
/// Five GOPs total (5 seconds of media). The first GOP is discarded as
/// startup below — a fresh encoder's first frame is a forced keyframe
/// with its own, legitimately different rate-control behavior — leaving
/// four full GOPs (4 seconds) as the measured steady-state window.
const BITRATE_TEST_TOTAL_FRAMES: u64 = BITRATE_TEST_GOP_FRAMES * 5;

/// Encodes `BITRATE_TEST_TOTAL_FRAMES` of the deterministic plasma
/// sequence at `bitrate_bps` and returns the ACHIEVED long-run bitrate
/// (bits per second) over the measured window: the first GOP (startup) is
/// discarded, and the remaining real aggregate encoded bits are divided
/// by the remaining real media duration — never a single frame's size.
fn measure_long_run_achieved_bps(bitrate_bps: u32) -> f64 {
    let cfg = config(bitrate_bps, BITRATE_TEST_GOP_FRAMES as u32);
    let cfg = EncoderConfig {
        effective_fps: BITRATE_TEST_EFFECTIVE_FPS,
        ..cfg
    };
    let mut encoder =
        Openh264SoftwareEncoder::with_config(BITRATE_TEST_WIDTH, BITRATE_TEST_HEIGHT, cfg)
            .unwrap_or_else(|error| panic!("construct the {bitrate_bps}bps encoder: {error}"));

    let mut measured_bytes: u64 = 0;
    for index in 0..BITRATE_TEST_TOTAL_FRAMES {
        let frame = plasma_rgb24_frame(BITRATE_TEST_WIDTH, BITRATE_TEST_HEIGHT, index);
        let unit = encoder
            .encode_raw(&frame)
            .unwrap_or_else(|error| panic!("encode frame {index} at {bitrate_bps}bps: {error}"));
        if index >= BITRATE_TEST_GOP_FRAMES {
            measured_bytes += unit.data.len() as u64;
        }
    }

    let measured_frames = (BITRATE_TEST_TOTAL_FRAMES - BITRATE_TEST_GOP_FRAMES) as f64;
    let measured_seconds = measured_frames / BITRATE_TEST_EFFECTIVE_FPS;
    (measured_bytes as f64 * 8.0) / measured_seconds
}

/// The real `EncoderConfig` bitrate-consumption proof. (Rewritten after
/// independent review found the original single-frame version measured
/// the wrong thing: bitrate alone cannot determine a QP window — QP
/// depends on bitrate RELATIVE TO resolution, frame rate, and content
/// complexity — so a fixed size ratio on one tiny 64×48 IDR was never
/// dimensionally sound evidence for "the configured bitrate genuinely
/// governs a stream.")
///
/// Three encoders, differing ONLY in `bitrate_bps`, each encode the SAME
/// deterministic, temporally coherent 5-second plasma sequence at a
/// realistic 640×480/15fps/1-second-GOP cadence — identical content,
/// frame rate, and keyframe cadence across all three. The startup GOP
/// (frame 0's forced keyframe and its immediate aftermath) is discarded;
/// the achieved bitrate is measured over the remaining 4 seconds (4 full
/// GOPs) of real, motion-bearing content as aggregate bits divided by
/// real media duration — never a single frame's size. A higher configured
/// target must yield a MATERIALLY higher achieved long-run rate at every
/// step (low < mid < high, each by a wide, characterised margin) — the
/// broad, tolerant signal a real rate controller earns over several GOPs
/// of genuine, reachable motion, never a fixed ratio.
#[test]
fn with_config_yields_a_materially_higher_long_run_achieved_bitrate_for_a_higher_configured_target()
{
    let low_bps = measure_long_run_achieved_bps(250_000);
    let mid_bps = measure_long_run_achieved_bps(1_000_000);
    let high_bps = measure_long_run_achieved_bps(4_000_000);

    assert!(
        low_bps > 0.0 && mid_bps > 0.0 && high_bps > 0.0,
        "sanity: every tier must produce SOME encoded bytes over the 4-second measured window \
         of real motion content (low={low_bps:.0}bps, mid={mid_bps:.0}bps, high={high_bps:.0}bps)"
    );
    // 30% is a broad, characterised tolerance: each configured target is
    // 4x the previous one, so a real rate controller genuinely responding
    // to it is expected to clear this by a wide margin — this only
    // guards against "roughly equal," not against undershooting the
    // configured target itself (a low-complexity/low-motion stream is
    // not obligated to fill an unpadded target, which is why this test's
    // content is chosen to carry real, continuous per-frame motion).
    assert!(
        mid_bps > low_bps * 1.3,
        "a 1Mbps target must achieve a materially higher long-run rate than a 250kbps target \
         over the identical 4-second measured window (low={low_bps:.0}bps achieved, \
         mid={mid_bps:.0}bps achieved) — an implementation that ignores the configured bitrate \
         would leave these roughly equal"
    );
    assert!(
        high_bps > mid_bps * 1.3,
        "a 4Mbps target must achieve a materially higher long-run rate than a 1Mbps target over \
         the identical 4-second measured window (mid={mid_bps:.0}bps achieved, \
         high={high_bps:.0}bps achieved) — an implementation that ignores the configured \
         bitrate would leave these roughly equal"
    );
}

/// The basic periodic-scheduling proof: with `keyframe_interval_frames`
/// set to 4, encoding 8 frames in a row (with no manual
/// `request_keyframe` call) must yield keyframes at output positions 0
/// and 4 only — a fresh encoder's first frame is always a keyframe, and
/// the SHARED encoder itself, not any caller, must fire the next one
/// automatically once 4 further frames have been successfully encoded. A
/// do-nothing / manual-only implementation (one that never fires a
/// keyframe unless `request_keyframe` is called) fails this outright:
/// position 4 would be a delta frame instead.
#[test]
fn encode_raw_fires_an_automatic_keyframe_every_configured_interval_with_no_manual_request() {
    let interval = 4;
    let mut encoder =
        Openh264SoftwareEncoder::with_config(WIDTH, HEIGHT, config(2_000_000, interval))
            .expect("construct the encoder with a 4-frame keyframe interval");

    let mut keyframe_positions = Vec::new();
    for index in 0..8u64 {
        let frame = noisy_rgb24_frame(WIDTH, HEIGHT, index);
        let unit = encoder
            .encode_raw(&frame)
            .unwrap_or_else(|error| panic!("encode frame {index}: {error}"));
        if unit.keyframe {
            keyframe_positions.push(index);
        }
    }

    assert_eq!(
        keyframe_positions,
        vec![0, 4],
        "with a 4-frame automatic interval and no manual request_keyframe call, keyframes must \
         land at output positions 0 (the always-keyframe first frame) and 4 (one full period \
         later) only, across two full periods of 8 encoded frames"
    );
}

/// The anti-cheat proof for capability 3: the periodic keyframe counter
/// must advance ONLY on a SUCCESSFULLY encoded output frame, never on an
/// attempted-but-failed input. With `keyframe_interval_frames = 3`: frame
/// 0 is the always-keyframe first frame (counter resets to 0); frame 1
/// succeeds (counter 1); a deliberately mis-dimensioned frame is then
/// ATTEMPTED and must fail with `Err` (per `encode_raw`'s own dimension
/// contract, mirroring `encode()`'s) — this attempt must NOT advance the
/// counter; frame 2 (a real, correctly sized frame) succeeds next and
/// must still be a DELTA frame, because only 2 real successes have
/// happened since the last keyframe, not 3. Frame 3 then completes the
/// real period and must be the automatic keyframe.
///
/// If an implementation counted ATTEMPTS instead of successes, the
/// sequence would be: frame0(kf,reset) → frame1(attempt 1) →
/// mis-dimensioned frame(attempt 2, even though it failed) → frame2
/// (attempt 3 ⇒ WRONGLY fires a keyframe here instead of staying delta) —
/// which is exactly the wrong output this test asserts against.
#[test]
fn a_failed_encode_attempt_never_advances_the_keyframe_interval_counter() {
    let interval = 3;
    let mut encoder =
        Openh264SoftwareEncoder::with_config(WIDTH, HEIGHT, config(2_000_000, interval))
            .expect("construct the encoder with a 3-frame keyframe interval");

    let frame0 = noisy_rgb24_frame(WIDTH, HEIGHT, 0);
    let unit0 = encoder.encode_raw(&frame0).expect("encode frame 0");
    assert!(
        unit0.keyframe,
        "frame 0 on a fresh encoder is always a keyframe"
    );

    let frame1 = noisy_rgb24_frame(WIDTH, HEIGHT, 1);
    let unit1 = encoder.encode_raw(&frame1).expect("encode frame 1");
    assert!(
        !unit1.keyframe,
        "frame 1 is only the first successful frame since the keyframe (count 1 of 3) — must \
         be a delta frame"
    );

    // A deliberately mis-dimensioned attempt: must fail without touching
    // the codec, and must NOT count toward the keyframe interval.
    let mismatched = RawFrame {
        width: WIDTH + 8,
        height: HEIGHT,
        format: PixelFormat::Rgb24,
        planes: vec![PlaneLayout {
            offset: 0,
            stride: ((WIDTH + 8) * 3) as usize,
        }],
        data: Bytes::from(vec![0u8; ((WIDTH + 8) * 3 * HEIGHT) as usize]),
    };
    let attempt_result = encoder.encode_raw(&mismatched);
    assert!(
        attempt_result.is_err(),
        "a mis-dimensioned frame must be REJECTED, never silently accepted or padded/cropped"
    );

    let frame2 = noisy_rgb24_frame(WIDTH, HEIGHT, 2);
    let unit2 = encoder.encode_raw(&frame2).expect("encode frame 2");
    assert!(
        !unit2.keyframe,
        "frame 2 must still be a delta frame: only 2 SUCCESSFUL frames have been encoded since \
         the last keyframe (the failed attempt in between must not have counted toward the \
         3-frame interval) — a keyframe here proves attempted inputs were wrongly counted"
    );

    let frame3 = noisy_rgb24_frame(WIDTH, HEIGHT, 3);
    let unit3 = encoder.encode_raw(&frame3).expect("encode frame 3");
    assert!(
        unit3.keyframe,
        "frame 3 is the 3rd SUCCESSFUL frame since the last keyframe and must complete the \
         real period as an automatic keyframe"
    );
}
