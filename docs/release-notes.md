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

Measured on the Home Assistant OS test VM, the add-on's detection fell back to
CPU at default settings — but that is a cold-start timing effect, not a
capability limit. The VM's virtualized Intel iGPU (ADL-N Intel Graphics) is
Vulkan-capable: given a longer probe window it completed a verified GPU
detection on `burn-wgpu` in about two and a half minutes. Its cold first shader
compile simply exceeds the default 60-second startup probe window, so at default
settings detection falls back to CPU with a receipt naming the measured time,
while hardware video decode stays active. Raising the `detection_probe_deadline_secs`
add-on option past that compile time activates accelerated detection on the same
VM.

Shipping the Vulkan runtime (the loader plus the arch's GPU ICDs) adds about
50 MB to each hardware image. The amd64 images ship the Intel and AMD Vulkan
drivers so one image accelerates on either vendor's iGPU/GPU; the aarch64
images ship the AMD, Broadcom, Panfrost, and Freedreno drivers. The aarch64
hardware image is build-proven only — no ARM hardware acceleration is promised
until it is measured on a real board.

The `vigil-static-musl` artifact remains the software-only, statically
linked build for hosts with no graphics device to map. Its download is
unchanged: it reports the honest CPU/software fallback and its doctor output
names the hardware image above as the upgrade path — it is not silently
swapped for a hardware build.
