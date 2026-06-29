ARG BUILD_FROM=ghcr.io/home-assistant/base:3.22
FROM ${BUILD_FROM}
ARG BUILD_ARCH
ARG BUILD_VERSION
LABEL io.hass.arch="${BUILD_ARCH}" \
      io.hass.type="app" \
      io.hass.version="${BUILD_VERSION}"
COPY vigil /usr/local/bin/vigil
# Bundle the Lovelace gallery card so startup can copy it to /config/www/
# and register it as a frontend resource without requiring manual steps.
COPY www/ /www/
ENV VIGIL_DATA_DIR=/data
ENV VIGIL_DROP_PRIVILEGES=1
ENV VIGIL_RUN_UID=1000
ENV VIGIL_RUN_GID=1000
EXPOSE 8099 8098
ENTRYPOINT ["vigil"]
CMD ["run"]
