# syntax=docker/dockerfile:1
ARG TARGETARCH

FROM busybox:1.37.0-musl AS busybox

FROM scratch AS amd64
COPY target/x86_64-unknown-linux-musl/release/vigil /usr/local/bin/vigil

FROM scratch AS arm64
COPY target/aarch64-unknown-linux-musl/release/vigil /usr/local/bin/vigil

FROM ${TARGETARCH} AS final
COPY --from=busybox /bin/busybox /bin/busybox
ENV PATH=/usr/local/bin
ENV VIGIL_DATA_DIR=/data
ENV VIGIL_DROP_PRIVILEGES=1
ENV VIGIL_RUN_UID=1000
ENV VIGIL_RUN_GID=1000
EXPOSE 8099
HEALTHCHECK --interval=5s --timeout=3s --start-period=1s --retries=3 CMD ["/bin/busybox", "wget", "-q", "-O", "-", "http://127.0.0.1:8099/health"]
ENTRYPOINT ["vigil"]
CMD ["run"]
