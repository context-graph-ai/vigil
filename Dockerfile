# syntax=docker/dockerfile:1
# The software-only image deliberately uses a tiny, pre-staged context. BuildKit
# supplies TARGETARCH for release multi-arch builds; ordinary Docker uses the
# amd64 default or the acceptance harness passes the host architecture.
FROM busybox:1.37.0-musl AS busybox

FROM scratch AS final
ARG TARGETARCH=amd64
COPY dist/docker/${TARGETARCH}/vigil /usr/local/bin/vigil
COPY --from=busybox /bin/busybox /bin/busybox
ENV PATH=/usr/local/bin
ENV VIGIL_DATA_DIR=/data
ENV VIGIL_DROP_PRIVILEGES=1
ENV VIGIL_RUN_UID=1000
ENV VIGIL_RUN_GID=1000
EXPOSE 8099
HEALTHCHECK --interval=5s --timeout=3s --start-period=1s --retries=3 CMD ["/bin/busybox", "wget", "-q", "-O", "-", "http://127.0.0.1:8099/health"]
# Split entrypoint/command so a plain `docker run` still runs `vigil run`, while
# `docker run <image> <subcommand>` execs `vigil <subcommand>`.
ENTRYPOINT ["/usr/local/bin/vigil"]
CMD ["run"]
