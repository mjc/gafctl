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
cargo nextest run --workspace --all-targets --locked -E 'test(TEST_NAME)'
python3 -m unittest discover -s tests -p test_gafctl_client.py
devenv tasks run check:ha-registry
devenv tasks run check:python-types
```

Replace `TEST_NAME` with a test name or substring. `check:ha-registry` requires
Linux. Test fixtures use fake Bluetooth transports, local HTTP servers, and
synthetic cloud data. Recorded device replies are in the
[Bluetooth reference](protocol-findings.md).

## Repository layout

| Path | Purpose |
| --- | --- |
| `src/lib.rs`, `src/main.rs`, `src/bin/gafctl.rs` | Shared application library and binary entrypoints |
| `src/arguments.rs`, `src/cli/` | Arguments, service commands, direct BLE commands and output |
| `src/server.rs`, `src/server/` | Server configuration, transport startup and shutdown |
| `src/service/` | Inventory, refresh, control replay and backend coordination |
| `src/backend/` | Persistent identities, state and per-device synchronization |
| `src/api.rs` | HTTP routes |
| `src/mqtt.rs`, `src/mqtt/` | Broker connection, commands, state and discovery |
| `crates/gafctl-api/` | Shared state, capability and command types |
| `crates/gafctl-client/` | HTTP client |
| `crates/gafctl-protocol/` | Original controller's wire format |
| `crates/gafctl-bluetooth/` | Bluetooth discovery and communication |
| `crates/gafctl-quickconnect/` | Cloud authentication, requests and decoding |
| `custom_components/gafctl/` | Home Assistant integration |
| `nix/` | Packages, NixOS module and checks |
| `fixtures/quickconnect/` | Synthetic cloud requests and responses |

Both binaries call the application library. On Unix, `gafctl server` replaces
itself with `gafctl-server`. Cargo features default to `cli`, `http`, and `mqtt`;
`mqtt` enables `http`. HTTP and MQTT commands use the same device service.
The protocol crate handles wire data; the Bluetooth and QuickConnect crates
handle transport. The HTTP client uses shared API types.

The HA integration requires Home Assistant 2026.9.4 or newer and Python 3.14.
`models.py` describes the API dictionaries. `readings.py` defines entities;
`controls.py` defines control bounds and confirmation. The coordinator checks
identity, ownership and capabilities, serializes controls, and refreshes state
for readback. Config entries store the server address and persistent device ID.

## Optional tools

```sh
bacon clippy
cargo llvm-cov nextest --workspace --html
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
and HA app image. Manual runs upload artifacts. A signed `vVERSION` tag publishes
a GitHub Release with checksums and `ghcr.io/mjc/gafctl:VERSION` through the
`release` environment. Set the GHCR package visibility to public after its first
publication.

For a release:

1. Set matching versions in the root Cargo manifest, HA integration manifest
   and app configuration. Write `docs/releases/VERSION.md` with the user-facing
   changes and installation requirements.
2. Commit runtime changes, then update `home-assistant/Dockerfile` to that source
   revision. The workflow compares its Cargo, toolchain, notice and runtime
   inputs against the release commit.
3. Run the Distribution workflow manually and check both architectures.
4. Merge the release changes into main. Create a signed annotated tag on that
   commit and push it:

   ```sh
   git tag -s v0.1.0 -m 'gafctl 0.1.0'
   git push origin v0.1.0
   ```

   Replace the version for later releases. GitHub must verify both the tag and
   commit signatures. The tagged commit must be on main, and the `release`
   environment must allow version tags.

The workflow checks the tag, commit, versions and release notes before building,
then rechecks the tag object and commit before publication. Distribution runs
share one concurrency group. Packages, archives and provenance files receive
checksums; GitHub release notes come from the checked-in version document.

On a publication retry, an existing published GitHub release must contain
identical assets. Different bytes fail instead of replacing published downloads.
An existing draft can have its assets replaced before publication. If a rebuild
changes artifact bytes, use the original artifacts or publish a new version.

Tagged builds attest the package and image archives. Verify a package with:

```sh
gh attestation verify --repo mjc/gafctl PATH_TO_PACKAGE
```

Registry manifests have no such attestation. Docker base images use pinned
index digests; apt dependencies resolve at build time.
