FROM ghcr.io/home-assistant/base:3.22

# Test-only sidecar. It joins the running add-on's PID namespace with
# SYS_PTRACE; no tracing package or extra privilege is added to the shipped
# Vigil image.
RUN apk add --no-cache busybox-extras strace \
    && test -x "$(command -v nc)" \
    && ln -s "$(command -v nc)" /usr/local/bin/vigil-trace-nc

ENTRYPOINT ["strace"]
