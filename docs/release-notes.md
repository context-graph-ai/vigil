# Vigil Release Notes

## Hardware acceleration across every install shape

Every installable Vigil artifact now carries its acceleration truth from one
manifest (`addons/vigil/runtime-packages.yaml`), baked into the image at
build time with nothing fetched at container start.

- generic_docker_hardware_artifact: vigil-generic-docker-hw-amd64, vigil-generic-docker-hw-arm64
- static_musl_artifact: vigil-static-musl

The hardware-enabled Home Assistant add-on and the hardware-enabled generic
Docker images (`vigil-generic-docker-hw-amd64` for amd64 hosts,
`vigil-generic-docker-hw-arm64` for aarch64 hosts) install GStreamer plus the
matching VA-API/Mesa runtime packages and decode video on the mapped graphics
device. They now also carry accelerated object detection: the shipped binary
builds with the `burn-wgpu` backend (wgpu → Vulkan), so a box with a capable
GPU accelerates detection, and a box with no usable GPU falls back to CPU
detection honestly — the receipt names why on `/health`, `vigil stats`, and
`vigil doctor acceleration`. The two add-on switches (`hardware_decoding`,
`accelerated_detection`) default on.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No adjacent artifact contract binds shipped GStreamer/Vulkan packages and achieved fallback reporting.` -->

Measured on the Home Assistant OS test VM, the add-on's detection starts on the
CPU and moves to the GPU when the preparation finishes. The VM's virtualized
Intel iGPU (ADL-N Intel Graphics) is Vulkan-capable: its cold first shader
compile takes about two and a half minutes, and it then completed a verified GPU
detection on `burn-wgpu`. Detection runs on the processor throughout that
preparation, with the surfaces naming it as under way, and hardware video decode
stays active; no setting bounds the preparation and none needs raising.

<!-- vigil-unenforced: classification=external-procedure; reason=`HAOS VM GPU timing and cold-shader behavior come from a measured physical run.` -->

Shipping the Vulkan runtime (the loader plus the arch's GPU ICDs) adds about
50 MB to each hardware image. The amd64 images ship the Intel and AMD Vulkan
drivers so one image accelerates on either vendor's iGPU/GPU; the aarch64
images ship the AMD, Broadcom, Panfrost, and Freedreno drivers. The aarch64
hardware image is build-proven only — no ARM hardware acceleration is promised
until it is measured on a real board.

<!-- vigil-unenforced: classification=external-procedure; reason=`Image-size and per-architecture driver contents require built-image inspection and measurement.` -->

The `vigil-static-musl` artifact remains the software-only, statically
linked build for hosts with no graphics device to map. Its download is
unchanged: it reports the honest CPU/software fallback and its doctor output
names the hardware image above as the upgrade path — it is not silently
swapped for a hardware build.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No adjacent artifact contract binds static-musl software-only behavior and upgrade guidance.` -->
