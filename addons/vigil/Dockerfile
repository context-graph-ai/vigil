# syntax=docker/dockerfile:1
#
# Built by Home Assistant Supervisor from this add-on folder as the build
# context: the folder carries the staged `vigil` binary (placed by the
# release pipeline or the harness before the rebuild) and the runtime
# package manifest. Nothing is compiled at install time and nothing is
# fetched at container start. The base image is the fixed
# ghcr.io/home-assistant/base:3.22 (matching runtime-packages.yaml).
FROM ghcr.io/home-assistant/base:3.22
ARG BUILD_ARCH
ARG BUILD_VERSION
LABEL io.hass.arch="${BUILD_ARCH}" \
      io.hass.type="app" \
      io.hass.version="${BUILD_VERSION}" \
      vigil.decode_backend="gstreamer" \
      vigil.detector_backend="burn-wgpu"
WORKDIR /build
COPY runtime-packages.yaml addons/vigil/runtime-packages.yaml
# PACKAGES is expanded from the tracked runtime-packages.yaml manifest,
# selecting the "addon" artifact's package list for the effective Home
# Assistant BUILD_ARCH — never a literal/duplicated list, so the store, the
# docs, and the diagnostics can never drift into different package stories.
RUN PACKAGES="$(awk -v artifact=addon -v arch="$BUILD_ARCH" '/^- name:/ { in_packages = 0 } /artifact:/ { seen_artifact = $2 == artifact } /arch:/ { seen_arch = $2 == arch } /packages:/ { in_packages = seen_artifact && seen_arch } in_packages && /^[[:space:]]*-/ { print $2 }' addons/vigil/runtime-packages.yaml)" && apk add --no-cache $PACKAGES
COPY vigil /usr/local/bin/vigil
# The Home Assistant base image's s6-overlay init scrubs the container
# environment before spawning the service by default, which would silence
# every VIGIL_* setting below — the privilege drop would never arm and the
# process would keep running as root. Keeping the environment is what makes
# the drop-to-unprivileged contract real inside the add-on.
ENV S6_KEEP_ENV=1
ENV VIGIL_DATA_DIR=/data
ENV VIGIL_DROP_PRIVILEGES=1
ENV VIGIL_RUN_UID=1000
ENV VIGIL_RUN_GID=1000
# The one runtime root both sides of the owner plane meet on. A container has no
# per-user runtime directory — nothing creates one for a service with no login
# session — so the package states the root instead of leaving the daemon to bind
# somewhere no command can find. It sits on the persistent data volume under
# VIGIL_DATA_DIR, so it survives a restart alongside the store it addresses, and
# it is an image ENV rather than something the service exports: that is what puts
# it in the environment of a separate `vigil why`/`events`/`stats` exec'd into
# this container as well as the daemon's own (S6_KEEP_ENV=1 above keeps it
# across the init that starts the service).
ENV CONTEXTDB_OWNER_READ_RUNTIME_DIR=/data/owner-plane
# Prepared at container init rather than at build time: /data is a volume
# mounted over the image, so the directory has to be created after that mount
# exists, and the service itself starts the vigil binary directly with no
# launcher in front of it.
COPY --chmod=755 provision-runtime-root.sh /etc/cont-init.d/10-runtime-root
EXPOSE 8099 8098
CMD ["/usr/local/bin/vigil", "run"]
