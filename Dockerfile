FROM docker.io/library/rust:1.98.1-slim-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends build-essential cmake pkg-config libdbus-1-dev dpkg-dev && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY src ./src
COPY crates ./crates
RUN cargo build --release --locked --bins

FROM build AS packages
ARG PACKAGE_ARCH=""
COPY packaging ./packaging
RUN packaging/package.sh "${PACKAGE_ARCH}"

FROM scratch AS artifacts
COPY --from=packages /src/dist/ /

FROM docker.io/library/debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libdbus-1-3 jq curl tini && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/gafctl /src/target/release/gafctl-server /usr/local/bin/
COPY home-assistant/run.sh /usr/local/bin/gafctl-entrypoint
ENV GAFCTL_IDENTITY_STORE=/data/identities.json
VOLUME /data
EXPOSE 8787
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s CMD curl --fail --silent http://127.0.0.1:8787/health >/dev/null || exit 1
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/gafctl-entrypoint"]
CMD ["gafctl-server", "--bind", "0.0.0.0:8787", "--allow-remote"]
