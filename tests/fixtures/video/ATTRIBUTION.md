# Real Video Fixtures

## `one-by-one-person-detection.mp4`

- Source: Intel IoT DevKit `sample-videos` repository.
- URL: `https://github.com/intel-iot-devkit/sample-videos/raw/master/one-by-one-person-detection.mp4`
- Source commit: `57978890822836f2b4743852f04f62fc511757e4`
- Upstream SHA-256: `a5964aa259099a482a8b360ffc2c57b5a30f84d5919236a4dad01f8e929ac07c`
- Local normalized SHA-256: `a65415f0da868f59014777ace1b702f6d7c6274c18e5af3e344cf710c37526ea`
- License: Creative Commons Attribution 4.0 International.
- Generation command: `cargo xtask setup-harness` verifies the source through `ffmpeg` as H.264
  Baseline L3.0.

This fixture is real recorded video used by the RTSP harness tests. It is not a generated pixel
fixture, frame manifest, or detector-answer source.

## Empty-Scene Derivative

The companion empty-scene segment is derived from the same upstream clip and license and verified by
`cargo xtask setup-harness`. It must be a carved segment from the upstream recording, not a generated
frame sequence or detector-answer fixture.

- `empty-scene-from-one-by-one-person-detection.mp4`
- Source: same Intel IoT DevKit upstream file and commit listed above.
- Local SHA-256: `2d4c35233e497d1c81d2a08187e856b5aba84acaf4f10cb47ccd33b9b5edee63`
- License: Creative Commons Attribution 4.0 International.
- Generation command: `cargo xtask setup-harness` verifies the source through `ffmpeg` as H.264
  Baseline L3.0 and writes the derivative from the quiet lead-in segment.

## YOLOX-Tiny COCO Checkpoint

- Local file: `tests/fixtures/models/yolox-tiny-coco.pth`
- Source: Megvii YOLOX release `0.1.1rc0`.
- URL: `https://github.com/Megvii-BaseDetection/YOLOX/releases/download/0.1.1rc0/yolox_tiny.pth`
- Local SHA-256: `9de513de589ac98bb92d3bca53b5af7b9acfa9b0bacb831f7999d0f7afaee8f0`
- License: Apache License 2.0, per the YOLOX upstream release and `tracel-ai/models/yolox-burn`
  notices.

This fixture is a frozen local model artifact for camera-loop tests. The runtime verifies the
checksum and never enables a pretrained-download path while watching the stream.
