# Development

Use the pinned devenv environment and Rust toolchain:

```sh
devenv allow
devenv shell
```

If `DEVENV_ROOT` points to this repository, run commands directly. If it points
elsewhere, start a fresh command from this repository's root.

Build both executables to run the server through the CLI:

```sh
cargo build --bins
./target/debug/gafctl server
```

For a CLI-only build:

```sh
cargo run --no-default-features --features cli --bin gafctl -- devices --format json
```

## Checks

```sh
devenv tasks run check:all
```

This runs Nix evaluation and formatting, Rust formatting, feature-specific
Clippy and tests, doctests, Python formatting/lint/types/client tests,
package/publication fixtures, and license checks. On Linux it also runs the
native Home Assistant registry suite with generated MQTT discovery fixtures.
The HA suite uses Home Assistant and dependencies from `devenv.lock` and `devenv.nix`.
The Nix task disables pip installation, matching Nix's Home Assistant launcher.
Running the Python test directly keeps Home Assistant's requirement checks enabled.

For focused checks:

```sh
cargo nextest run --all-targets --locked -E 'test(TEST_NAME)'
python3 -m unittest discover -s tests -p test_gafctl_client.py
devenv tasks run check:ha-registry
devenv tasks run check:python-types
```

Replace `TEST_NAME` with a test name or substring. `check:ha-registry` requires
Linux. Test fixtures use fake Bluetooth transports, local HTTP servers, and
synthetic cloud data. Recorded device replies are in the
[Bluetooth reference](reference.md#captured-device-evidence).

## Code

`src/` contains one Rust package with the server, CLI, device coordination, HTTP,
and MQTT. Bluetooth, protocol, shared models, the HTTP client, and QuickConnect
live in their own modules. `custom_components/gafctl/` is the HA integration;
`nix/` and `packaging/` provide installations. Cloud fixtures are synthetic.

Cargo defaults to `cli`, `http`, and `mqtt`; MQTT enables HTTP. Use
`--no-default-features --features http` for a server without MQTT, or `cli` for
the CLI alone. Both binaries call the application library.

The HA integration requires Home Assistant 2026.9.4 or newer and Python 3.14.
API types are in `models.py`, entities in `readings.py`, and control bounds/confirmation in
`controls.py`. The coordinator handles identity, ownership, serialization, and
readback. For Rust callers, `gafctl::client` uses the shared `gafctl::model` types.

## Optional tools

```sh
bacon clippy
cargo llvm-cov nextest --html
cargo deny check advisories sources
cargo machete
cargo tree --duplicates
```

## Installation checks

```sh
devenv tasks run check:install
devenv tasks run check:compose
devenv tasks run check:install-native
```

`check:install` uses Podman by default. For Docker, set `CONTAINER_ENGINE=docker`
and `DOCKER_HOST`. It builds packages and images for the runtime's native
architecture, tests Ubuntu 24.04 and Debian 12 with systemd, and checks image
startup, secrets, restart and shutdown. The privileged test containers use
synthetic configuration and have no Bluetooth devices or host D-Bus socket.
The task removes its containers and volumes on exit.

Run `check:compose` against the same Docker runtime after building the image.
Set `GAFCTL_CHECK_SECRET_DIR` to an empty directory on the daemon's Linux host
to check the password bind mount with a synthetic secret. `check:install-native`
installs both binaries into `target/install-native` and checks an empty server.

For Home Assistant OS, install the app in a disposable guest. Copy
`packaging/check-haos.sh` to its writable data partition and run it with the
Supervisor app slug. It checks startup, restart, watchdog recovery, and cold
backup/restore. Install the integration and complete its config flow against a
local API fixture. Run on both `amd64` and `aarch64`.

## Nix checks

```sh
devenv tasks run check:nix
devenv shell -- nix build .#gafctl
devenv shell -- python3 packaging/check-native.py --package ./result
```

`check:nix` evaluates all supported systems and checks Nix formatting. Module
checks cover disabled, Bluetooth, cloud, MQTT, and mixed configurations,
credentials, secret paths and listener settings. Package builds run Rust tests
in the Nix sandbox.

On a disposable NixOS host with a `gafctl` system account and BlueZ policy,
run `packaging/check-nix-service.sh` in the devenv shell. It tests a generated
unit with a private broker, credentials, permissions, HTTP, restart and identity
persistence, then removes its units and fixtures. It reads BlueZ's object list
without scanning or connecting. Set `GAFCTL_CHECK_SUDO=doas` to use doas.

## Releases

After changing dependencies or the Rust toolchain, regenerate the notices:

```sh
devenv tasks run licenses:update
```

`cargo-about` generates `THIRD-PARTY-NOTICES.txt`; the pinned Rust toolchain
supplies `LICENSE-RUST-STDLIB.html`. All distributions include both, the project
license and the QuickConnect reference notice. `check:licenses` checks coverage
and freshness. Upstream license references are described in
[packaging/license-reference](../packaging/license-reference/README.md).

The [Distribution workflow](../.github/workflows/release.yml) runs native AMD64
and ARM64 checks before building and testing the exported packages, server image
and HA app image. `check:package` builds the Cargo archive with its registry dependencies. Manual runs upload artifacts. A signed `vVERSION`
tag publishes `gafctl` to crates.io, a GitHub Release with checksums,
and `ghcr.io/mjc/gafctl:VERSION` through the `release` environment. The Cargo
installation workflow then installs the published version on AMD64 and ARM64
and checks both binaries, server startup, HTTP, and shutdown.

The crate's trusted publisher must allow repository `mjc/gafctl`, workflow
`release.yml`, and environment `release`. Authentication uses GitHub OIDC.
For the first publication, set the release environment's `CRATES_IO_TOKEN`
secret to a token that can create `gafctl`. Configure its trusted publisher
after publication, then remove that secret.
Set the GHCR package visibility to public after its first publication.

The [Home Assistant workflow](../.github/workflows/home-assistant.yml) runs HACS
and Hassfest validation on pull requests, main, and release candidates. HACS
checks skip branding while the integration is available as a custom repository.
Before submitting to the default HACS catalog, register `gafctl` with
`home-assistant/brands` and remove that exception.

HACS reads the GitHub release tag and installs `custom_components/gafctl` from
that revision. No separate upload or integration ZIP is required. The release
version check includes the integration manifest.

For a release:

1. Set matching versions in the Cargo manifest, HA integration manifest,
   and app configuration. Write `docs/releases/VERSION.md` with the user-facing
   changes and installation requirements. Update the versioned installation
   commands.
2. Commit runtime changes, then update `home-assistant/Dockerfile` to that source
   revision. The workflow compares its Cargo, toolchain, notice and runtime
   inputs against the release commit.
3. Run the Distribution workflow manually and check both architectures.
4. Merge the release changes into main. Create a signed annotated tag on that
   commit and push it:

   ```sh
   git tag -s v0.1.1 -m 'gafctl 0.1.1'
   git push origin v0.1.1
   ```

   Replace the version for later releases. GitHub must verify both the tag and
   commit signatures. The tagged commit must be on main, and the `release`
   environment must allow version tags.

Publication rechecks signatures and versions. Published GitHub assets must have
exactly the expected names and identical bytes on retry; drafts may replace
expected assets but reject unexpected ones. Published prereleases are rejected.
Registry tags must match image configurations and the complete architecture set;
identical images are skipped. Authentication/network errors stop publication.
If rebuilt bytes differ, use the original artifacts or publish a new version.
Cargo publication checks the version’s checksum if it already exists, publishes
it otherwise, then compares the downloaded archive with the local one.

`check:release` tests package validation, signed-candidate checks, publication
retries, and failure handling. Git repositories are local fixtures; GitHub and
Docker responses are stubbed. These tests make no external writes.

Tagged builds attest the package and image archives. Verify a package with:

```sh
gh attestation verify --repo mjc/gafctl PATH_TO_PACKAGE
```

Registry manifests have no such attestation. Docker base images use pinned
index digests; apt dependencies resolve at build time.

### Build distribution files locally

After a release binary build, install `dpkg-dev` and `jq` on Debian/Ubuntu,
then run `./packaging/package.sh`. The files appear in `dist/`.
For Docker Buildx, run:

```sh
docker buildx build --target artifacts --output type=local,dest=dist .
```

This emits a Debian package and archive for the native architecture. With
Podman, build `--target packages` and copy `/src/dist` out of the image.
Use native ARM64 for ARM64 builds; no emulation is configured.
