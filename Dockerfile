# syntax=docker/dockerfile:1
# TARGETARCH is a BuildKit-predefined build arg (auto-populated per
# --platform target) usable directly in FROM lines with no ARG declaration
# needed. A leading ARG line before the first FROM is deliberately avoided
# here so every stage starts cleanly with FROM.
FROM busybox:1.37.0-musl AS busybox

FROM scratch AS amd64
COPY target/x86_64-unknown-linux-musl/release/vigil /usr/local/bin/vigil

FROM scratch AS arm64
COPY target/aarch64-unknown-linux-musl/release/vigil /usr/local/bin/vigil

# --- Hardware-enabled generic Docker images -------------------------------
#
# Builder stages compile vigil from source with
# --features decode-gstreamer,detect-burn-wgpu (native, dynamically linked;
# never the musl/scratch static build below); the paired runtime stages
# install exactly the matching arch's runtime-packages.yaml package set
# literally via apk (no toolchain/dev packages ship). These images carry the
# accelerated (wgpu -> Vulkan) detector (burn-wgpu) and fall back to CPU
# honestly on a box with no usable GPU.
# These are named build targets the release pipeline publishes
# (`docker build --target vigil-generic-docker-hw-amd64 .`) from an
# ASSEMBLED context directory that carries this repo plus its sibling
# source trees side by side (the workspace path dependencies in Cargo.toml
# resolve one level up: context-graph/crates/... and contextdb/crates/...):
#   context/
#     vigil/           (this repo)
#     context-graph/
#     contextdb/
#   docker build --target vigil-generic-docker-hw-amd64 -f vigil/Dockerfile context/
# not a plain checkout of this repo alone. The software-only static image
# below stays the default `docker build .` result so a plain checkout
# always builds.
FROM rust:alpine3.22 AS vigil-hw-builder-amd64
RUN apk add --no-cache build-base pkgconf gstreamer-dev gst-plugins-base-dev
COPY vigil/ /workspace/vigil-src/
COPY context-graph/ /workspace/context-graph/
COPY contextdb/ /workspace/contextdb/
WORKDIR /workspace/vigil-src
# The repo's cargo config targets the static-musl artifacts; the hardware
# build must link Alpine's real gstreamer/mesa shared libraries, so the
# config moves aside and crt-static is disabled for this build stage.
RUN mv .cargo/config.toml /tmp/vigil-cargo-cross-config.toml
ENV RUSTFLAGS="-C target-feature=-crt-static"
RUN cargo build --release --bin vigil --features decode-gstreamer,detect-burn-wgpu --locked

FROM alpine:3.22 AS vigil-generic-docker-hw-amd64
RUN apk add --no-cache \
    gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad gst-vaapi \
    libva libva-intel-driver intel-media-driver mesa-va-gallium \
    vulkan-loader mesa-vulkan-intel mesa-vulkan-ati
COPY --from=vigil-hw-builder-amd64 /workspace/vigil-src/target/release/vigil /usr/local/bin/vigil
LABEL vigil.artifact="vigil-generic-docker-hw-amd64" \
      vigil.arch="amd64" \
      vigil.decode_backend="gstreamer" \
      vigil.detector_backend="burn-wgpu"
ENV VIGIL_DATA_DIR=/data
ENV VIGIL_DROP_PRIVILEGES=1
ENV VIGIL_RUN_UID=1000
ENV VIGIL_RUN_GID=1000
EXPOSE 8099 8098
CMD ["/usr/local/bin/vigil", "run"]

FROM rust:alpine3.22 AS vigil-hw-builder-arm64
RUN apk add --no-cache build-base pkgconf gstreamer-dev gst-plugins-base-dev
COPY vigil/ /workspace/vigil-src/
COPY context-graph/ /workspace/context-graph/
COPY contextdb/ /workspace/contextdb/
WORKDIR /workspace/vigil-src
# The repo's cargo config targets the static-musl artifacts; the hardware
# build must link Alpine's real gstreamer/mesa shared libraries, so the
# config moves aside and crt-static is disabled for this build stage.
RUN mv .cargo/config.toml /tmp/vigil-cargo-cross-config.toml
ENV RUSTFLAGS="-C target-feature=-crt-static"
RUN cargo build --release --bin vigil --features decode-gstreamer,detect-burn-wgpu --locked

FROM alpine:3.22 AS vigil-generic-docker-hw-arm64
RUN apk add --no-cache \
    gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad \
    libva mesa-va-gallium mesa-dri-gallium \
    vulkan-loader mesa-vulkan-ati mesa-vulkan-broadcom mesa-vulkan-panfrost mesa-vulkan-freedreno
COPY --from=vigil-hw-builder-arm64 /workspace/vigil-src/target/release/vigil /usr/local/bin/vigil
LABEL vigil.artifact="vigil-generic-docker-hw-arm64" \
      vigil.arch="aarch64" \
      vigil.decode_backend="gstreamer" \
      vigil.detector_backend="burn-wgpu"
ENV VIGIL_DATA_DIR=/data
ENV VIGIL_DROP_PRIVILEGES=1
ENV VIGIL_RUN_UID=1000
ENV VIGIL_RUN_GID=1000
EXPOSE 8099 8098
CMD ["/usr/local/bin/vigil", "run"]

# Default build target: the software-only static image. Keeps a plain
# `docker build .` working on any checkout with the prebuilt musl binaries.
FROM ${TARGETARCH} AS final
COPY --from=busybox /bin/busybox /bin/busybox
ENV PATH=/usr/local/bin
ENV VIGIL_DATA_DIR=/data
ENV VIGIL_DROP_PRIVILEGES=1
ENV VIGIL_RUN_UID=1000
ENV VIGIL_RUN_GID=1000
EXPOSE 8099
HEALTHCHECK --interval=5s --timeout=3s --start-period=1s --retries=3 CMD ["/bin/busybox", "wget", "-q", "-O", "-", "http://127.0.0.1:8099/health"]
# Split entrypoint/command so a plain `docker run` still runs `vigil run`, while
# `docker run <image> <subcommand>` execs `vigil <subcommand>` instead of a
# literal binary named after the argument.
ENTRYPOINT ["/usr/local/bin/vigil"]
CMD ["run"]
