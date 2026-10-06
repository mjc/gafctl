FROM docker.io/library/rust:1.98.1-slim-bookworm@sha256:ff521445a372125ed4f76e1453a1f8098f2d05332d1601d30db1c1f62757e730 AS build
RUN apt-get update && apt-get install -y --no-install-recommends build-essential cmake pkg-config libdbus-1-dev dpkg-dev jq && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY src ./src
COPY crates ./crates
RUN cargo build --release --locked --bins

FROM build AS packages
ARG PACKAGE_ARCH=""
COPY packaging ./packaging
COPY LICENSE LICENSE-QUICKCONNECT-REFERENCE.txt THIRD-PARTY-NOTICES.txt LICENSE-RUST-STDLIB.html ./
COPY custom_components/gafctl/manifest.json ./custom_components/gafctl/manifest.json
COPY home-assistant/config.yaml ./home-assistant/config.yaml
RUN packaging/package.sh "${PACKAGE_ARCH}"

FROM scratch AS artifacts
COPY --from=packages /src/dist/ /

FROM docker.io/library/debian:bookworm-slim@sha256:3783cc01769c7b2b1b83a5c5ad96c815348e28ed7da68e2e3687004faa906251 AS runtime
ARG GAFCTL_REVISION=unknown
LABEL org.opencontainers.image.revision="${GAFCTL_REVISION}"
LABEL org.opencontainers.image.licenses="MIT"
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libdbus-1-3 jq curl tini && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/gafctl /src/target/release/gafctl-server /usr/local/bin/
COPY LICENSE LICENSE-QUICKCONNECT-REFERENCE.txt THIRD-PARTY-NOTICES.txt LICENSE-RUST-STDLIB.html /usr/share/licenses/gafctl/
COPY home-assistant/run.sh /usr/local/bin/gafctl-entrypoint
ENV GAFCTL_IDENTITY_STORE=/data/identities.json
VOLUME /data
EXPOSE 8787
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s CMD curl --fail --silent http://127.0.0.1:8787/health >/dev/null || exit 1
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/gafctl-entrypoint"]
CMD ["gafctl-server", "--bind", "0.0.0.0:8787", "--allow-remote"]
