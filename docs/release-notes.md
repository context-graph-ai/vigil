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
device; object detection stays on the CPU (`burn-cpu`) in every shipped shape
this run — the two add-on switches (`hardware_decoding`, `accelerated_detection`)
default on, and the add-on option help says plainly that detection runs on
CPU in this artifact today.

The `vigil-static-musl` artifact remains the software-only, statically
linked build for hosts with no graphics device to map. It reports the honest
CPU/software fallback and its doctor output names the hardware image above
as the upgrade path — it is not silently swapped for a hardware build.
