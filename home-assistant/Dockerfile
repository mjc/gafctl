ARG GAFCTL_REVISION=45c61847d699eecb97de3f9aa27f4137f4be22ba
FROM docker.io/library/rust:1.98.1-slim-bookworm@sha256:ff521445a372125ed4f76e1453a1f8098f2d05332d1601d30db1c1f62757e730 AS build
RUN apt-get update && apt-get install -y --no-install-recommends build-essential cmake pkg-config libdbus-1-dev curl && rm -rf /var/lib/apt/lists/*
ARG GAFCTL_REVISION
WORKDIR /src
RUN curl --fail --location --output source.tar.gz "https://github.com/mjc/gafctl/archive/${GAFCTL_REVISION}.tar.gz" && tar -xzf source.tar.gz --strip-components=1 && rm source.tar.gz
RUN cargo build --release --locked --bins

FROM docker.io/library/debian:bookworm-slim@sha256:3783cc01769c7b2b1b83a5c5ad96c815348e28ed7da68e2e3687004faa906251
ARG BUILD_VERSION
ARG BUILD_ARCH
ARG GAFCTL_REVISION
LABEL io.hass.version="${BUILD_VERSION}" io.hass.type="app" io.hass.arch="${BUILD_ARCH}" org.opencontainers.image.revision="${GAFCTL_REVISION}" org.opencontainers.image.licenses="MIT"
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libdbus-1-3 jq curl && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/gafctl /src/target/release/gafctl-server /usr/local/bin/
COPY --from=build /src/LICENSE /src/LICENSE-QUICKCONNECT-REFERENCE.txt /src/THIRD-PARTY-NOTICES.txt /src/LICENSE-RUST-STDLIB.html /usr/share/licenses/gafctl/
COPY run.sh /usr/local/bin/gafctl-entrypoint
ENV GAFCTL_IDENTITY_STORE=/data/identities.json
EXPOSE 8787
ENTRYPOINT ["/usr/local/bin/gafctl-entrypoint"]
CMD ["gafctl-server", "--bind", "0.0.0.0:8787", "--allow-remote"]
