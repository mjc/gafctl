FROM docker.io/library/rust:1.98.1-slim-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends build-essential cmake pkg-config libdbus-1-dev curl && rm -rf /var/lib/apt/lists/*
# Pin the server source independently of the app's configuration and launcher.
ARG GAFCTL_REVISION=352e15d22704f153a7433ea3ecf240d6089af6eb
WORKDIR /src
RUN curl --fail --location --output source.tar.gz "https://github.com/mjc/gafctl/archive/${GAFCTL_REVISION}.tar.gz" && tar -xzf source.tar.gz --strip-components=1 && rm source.tar.gz
RUN cargo build --release --locked --bins

FROM docker.io/library/debian:bookworm-slim
ARG BUILD_VERSION
ARG BUILD_ARCH
LABEL io.hass.version="${BUILD_VERSION}" io.hass.type="app" io.hass.arch="${BUILD_ARCH}"
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libdbus-1-3 jq curl && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/gafctl /src/target/release/gafctl-server /usr/local/bin/
COPY run.sh /usr/local/bin/gafctl-entrypoint
ENV GAFCTL_IDENTITY_STORE=/data/identities.json
EXPOSE 8787
ENTRYPOINT ["/usr/local/bin/gafctl-entrypoint"]
CMD ["gafctl-server", "--bind", "0.0.0.0:8787", "--allow-remote"]
