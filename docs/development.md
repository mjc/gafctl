# Development

Use the repository's pinned devenv environment. Rust is pinned in
`rust-toolchain.toml`.

```sh
devenv allow
devenv shell
```

Run the CLI inside that shell:

```sh
cargo run --no-default-features --features cli --bin gafctl -- ble scan
cargo run --no-default-features --features cli --bin gafctl -- devices --format json
```

Start the server directly with `cargo run --bin gafctl-server --`, using the
configuration in the deployment guide. To run `gafctl server`,
build both executables first:

```sh
cargo build --bins
./target/debug/gafctl server
```

If `DEVENV_ROOT` points to this repository, run Cargo commands directly. If it
points elsewhere, start a fresh command from this repository's root.

## Checks

```sh
devenv tasks run check:all
```

This runs Nix evaluation and formatting, Clippy with warnings denied for all features, CLI-only
and HTTP-only configurations, workspace tests with all features, CLI-only and
HTTP-only tests, Rust doctests, Ruff formatting and lint checks, strict typing of the Python
client, models and controls, and the Python
Home Assistant client tests and release image publication tests.

`check:release` uses a local Docker stub to check artifact validation,
architecture agreement, image tags and manifest members. It also runs package boundary tests
that reject mismatched ELF targets,
non-ELF input and stale CLI/server versions before packaging. Neither suite
publishes images.

On native Linux, `check:all` also runs `check:ha-registry`. That task uses Home
Assistant and MQTT dependencies pinned by `devenv.lock`, generates discovery
fixtures from Rust, and checks HA entity/device registries,
onboarding, reconfiguration, ownership transitions and MQTT templates. Each run
uses a configuration directory under `target/ha-registry` and runs natively.
The suite uses synthetic data and has no connection to the deployed HA or fan.

Run that suite separately on native Linux:

```sh
devenv tasks run check:ha-registry
```

The HA package and registry task require Linux. Other checks run on both platforms.

For a focused Rust test inside the shell:

```sh
cargo nextest run --workspace --all-targets --locked -E 'test(TEST_NAME)'
```

Replace `TEST_NAME` with a test name or substring. For the Python tests:

```sh
python3 -m unittest discover -s tests -p test_gafctl_client.py
```

The HA integration baseline is Home Assistant 2026.9.4 or newer, using Python 3.14 and typed config-entry runtime data.
`models.py` describes the fixed Rust API dictionaries with `TypedDict`.
The client returns those dictionaries directly, checking device identity,
backend agreement, ownership, availability and command confirmation.
The server owns scalar types and bounds.

`readings.py` defines sensor and binary-sensor metadata for each backend.
Entity eligibility and construction use those same definitions. The metadata
module has no Home Assistant dependency; platform modules apply HA enums and
presentation behavior.
`controls.py` defines control eligibility, setting bounds and readback checks.
The coordinator validates HA input, checks current ownership and capabilities,
sends once under a lock, and refreshes state after the response.
Config entries store the server address and device identity; capabilities come
from inventory. Initial setup resolves inventory once through the coordinator.
Entities use that coordinator for identity, availability and controls.

Run `devenv tasks run check:python-types` for strict typing. Format and lint Python changes with
`ruff format custom_components tests/*.py` and
`ruff check custom_components tests/*.py`.

Tests use fake Bluetooth transports, local HTTP servers, and synthetic cloud
fixtures. They check parsing, request validation, timeouts, state handling,
controls, and Home Assistant mapping. Hardware captures are documented in
[protocol findings](protocol-findings.md). QuickConnect tests use synthetic data;
live account compatibility is untested.

## Repository layout

| Path | Purpose |
| --- | --- |
| `src/lib.rs` | Application entrypoints and shared process exit handling |
| `src/main.rs`, `src/bin/gafctl.rs` | Tokio entrypoints for the two executables |
| `src/arguments.rs` | Shared positive, platform-representable deadline arguments |
| `src/cli/` | Administrative command dispatch, service commands, direct BLE commands, and text/JSON output |
| `src/server.rs`, `src/server/` | Server startup, configuration, secret loading, and diagnostic commands |
| `src/service/` | Inventory and ownership, refresh workers, control replay, configured backends, and typed state publication |
| `src/api.rs` | HTTP routing and request/response adaptation |
| `src/backend/` | Persistent device identities, inventory, current state, and per-device coordination |
| `src/mqtt.rs`, `src/mqtt/` | Broker connection, request translation, delivery tracking, retained state, and Home Assistant discovery |
| `crates/gafctl-api/` | Shared device, state, capability, and command types |
| `crates/gafctl-client/` | HTTP client library for a running service |
| `crates/gafctl-protocol/` | Original controller's frames, commands, and values |
| `crates/gafctl-bluetooth/` | Bluetooth discovery and communication |
| `crates/gafctl-quickconnect/` | Cloud authentication, requests, and decoding |
| `custom_components/gafctl/` | Home Assistant integration |
| `nix/` | Package, Home Assistant component, NixOS module, and configuration checks |
| `fixtures/quickconnect/` | Synthetic cloud request and response examples |

The protocol crate has no Bluetooth or application dependency. The HTTP client
uses the shared API types without importing the service or Bluetooth runtime.

Both binaries call the root application library. `gafctl server` replaces the
CLI process with `gafctl-server` on Unix; it does not construct a service inside
the CLI. The `cli` feature includes administrative commands and direct BLE.
The `http` feature includes the service and server configuration; `mqtt` adds
the broker adapter and enables `http`. Each feature gates its application
modules in `src/lib.rs`.

CLI service and BLE commands share their argument values and output format.
Administrative and diagnostic commands use the same deadline parser.
BLE parsing, transport options, result projection, and command exit decisions
live together in `src/cli/ble.rs`. Server argument parsing constructs validated
configuration from `src/server/config.rs`; device execution does not depend on
CLI parser types. The HTTP and MQTT adapters call the same device service.

Server startup owns transport assembly. `src/server/mqtt.rs` attaches the typed
state publisher before cloning the service for request workers, then owns
intake and drain handles. `src/mqtt/adapter.rs` translates command and refresh
envelopes into service calls. The service uses shared API types and backend
operations without importing server configuration or broker types.

`src/service/inventory.rs` owns registered-device views and persisted entity
ownership. `src/service/refresh.rs` owns detached reads and overlapping refresh
coordination; the rendezvous channel remains on each device runtime.
`src/service/quickconnect/` owns one configured cloud backend with its client,
account identity, registry, and readback policy. Its read and control operations
share that configuration. It normalizes provider inventory and constructs detail
read targets. The registry receives normalized identities and owns persistence,
missing-device status, and read generations. Polling and manual refresh share
generation-checked detail commits; control readback uses control generations.
State publication serializes snapshot collection and
replacement and rejects snapshots with obsolete ownership descriptors.

Each registry entry owns its descriptor and runtime. Re-registration updates
the descriptor while retaining state, locks, and generation tracking.
Runtime observations are unknown, an available state, or a typed unavailable
reason. Snapshot projection derives inventory status and error text and checks
freshness before cloning an available payload.
API commands define their required capability, and capabilities define their
backend. Registry dispatch checks both before selecting a runtime. The configured
QuickConnect backend translates API commands into provider writes and rechecks
the original command's permission before sending.
Control history stores command identities and outcomes; the service constructs
correlated responses for callers. MQTT intake owns admission and reply tasks
under one lock, which is released before shutdown awaits those tasks.
MQTT publications distinguish retained, untracked, and acknowledged delivery
with enum variants. Refresh and rejection replies retain validated command IDs
through serialization.

MQTT number controls share setting bounds, units, and command fields, with
explicit backend differences. Device discovery shares metadata at the root and
uses the same component catalogue for publication, capability removal, and
cleanup of individual discovery topics during upgrades.

Shared legacy snapshot normalization lives in `src/legacy_projection.rs`.
Direct BLE output retains partial readings and field errors; the service
accepts complete decoded snapshots before publishing current state. Each
caller owns that acceptance decision. Readback presentation,
stdout writing, and logging also have one implementation in the application
library. Shared request and response types belong to `gafctl-api`, transport I/O to
the Bluetooth and QuickConnect crates, and wire decoding to `gafctl-protocol`.

Unit tests live beside the parser, projection, configuration, runtime, or
adapter they exercise. `tests/cli.rs` checks actual executable startup,
argument forwarding, stdout, exit codes, and HTTP command behavior. The
CLI-only test task includes the library target as well as the executable.
Per-crate tests cover wire and client contracts. The Python client tests use
an in-memory HTTP session; the native Linux registry suite uses pinned Home
Assistant and generated MQTT discovery fixtures. Shared test fixtures stay
local to these boundaries.

## Optional tools

Run these inside the development shell as needed:

```sh
bacon clippy
cargo llvm-cov nextest --workspace --html
cargo deny check advisories sources
cargo machete
cargo tree --duplicates
```

## Distribution

`cargo-about` generates `THIRD-PARTY-NOTICES.txt` for all server/CLI features and
supported targets. `LICENSE-RUST-STDLIB.html` preserves the pinned Rust standard
library's upstream notices. Both files ship with every binary distribution.
After changing dependencies or the Rust toolchain, regenerate them:

```sh
devenv tasks run licenses:update
```

`check:licenses` rejects missing source license text and stale notices. Add a
checksummed clarification in `about.toml` when an upstream composite license
needs explicit treatment; preserve its full copyright text.

The root Dockerfile builds both binaries with the checked-in toolchain and
lockfile. Its `artifacts` target exports native Linux `.deb` packages and binary
archives; `runtime` runs the server. The Home Assistant app builds the same Rust
source from the revision pinned in `home-assistant/Dockerfile`.

The Distribution workflow builds on native x86-64 and ARM64 runners. A manual
run uploads build artifacts without publishing. A `vVERSION` tag must match the
root Cargo version, HA manifest version, and app version. Update the app source
revision when releasing server changes.

Release publication requires successful checks and artifact tests, an immutable
`vVERSION` tag, and the GitHub `release` environment restricted to version tags.
Required environment reviewers can be added separately. Its publish job creates a GitHub Release with
checksums and publishes `ghcr.io/mjc/gafctl:VERSION` as a multi-architecture
image. Set the GHCR package visibility to public after its first publication.
Do not document a registry tag as available before it has been published.

## Installation checks

Use a disposable Linux container runtime. `check:install` builds the Linux
archive, Debian package, server image, and Home Assistant app image for the
runtime's native architecture. It installs the package on Ubuntu 24.04 and
Debian 12 with systemd as PID 1, then checks credentials, service permissions,
archive startup, restart, reinstall, upgrade, and purge. Image checks cover
HTTP startup, option mapping, private password files, restart, and shutdown.
The privileged containers in this check have no Bluetooth devices or host
D-Bus socket mounted.

```sh
devenv tasks run check:install
```

Podman is the default engine. To use a disposable Docker daemon, set
`CONTAINER_ENGINE=docker` and `DOCKER_HOST` before running the task. Named test
volumes carry the fixtures, so a remote daemon does not need the checkout
mounted. The task removes its test containers and volumes when it exits.

For Compose, point `DOCKER_HOST` at that same test runtime after the image
build. `GAFCTL_CHECK_SECRET_DIR` is an empty directory on the daemon's Linux
host; setting it also tests the QuickConnect password bind mount with a
synthetic secret and no cloud requests.

```sh
GAFCTL_CHECK_SECRET_DIR=/var/lib/gafctl-compose-check devenv tasks run check:compose
devenv tasks run check:install-native
```

The native check installs both executables with Cargo into `target/install-native`
and starts `gafctl server` with an empty device inventory and an identity
store under that directory. It does not inherit device or cloud settings from the shell.

For Home Assistant OS, add this repository to a disposable guest's app store
and install Gafctl through Supervisor. Copy `packaging/check-haos.sh` onto the
guest's writable data partition, then run it with the app slug shown by
Supervisor. This checks startup, restart, watchdog recovery after a forced
container stop, and cold backup/restore of the identity store and a private
synthetic file. Install the separate integration, restart Home Assistant, and
complete its config flow against a local API fixture to check entity loading.
Run these checks on both `amd64` and `aarch64` guests. Use Nix-provided QEMU and
mtools for guest setup, with native hardware acceleration.

## Nix checks

`check:nix` evaluates every supported flake system and checks Nix formatting.
The module checks cover disabled, Bluetooth, cloud, MQTT, and mixed services;
they reject missing credentials, passwords in the store, and unsafe listener
settings. Package builds run the Rust tests in the Nix sandbox.

```sh
devenv tasks run check:nix
devenv shell -- nix build .#gafctl
devenv shell -- python3 packaging/check-native.py --package ./result
```

On a disposable NixOS test host with an existing `gafctl` system account and
BlueZ policy, run `packaging/check-nix-service.sh` inside the devenv shell.
It builds the module's generated test unit, reads its fixture metadata, starts a
private MQTT broker, and checks credential loading, HTTP, service permissions and identity persistence
across restart, then removes its units and fixtures. The artifact defines the
test ports, paths and unit names. It never configures a fan or cloud account.
Set `GAFCTL_CHECK_SUDO=doas` if that is the host's privilege tool. The check reads BlueZ's object list as the
service account without scanning or connecting to a device.

## Release artifacts

The distribution workflow runs the repository's pinned devenv `check:all` on
native amd64 and arm64, including Rust feature configurations and native Home
Assistant registry tests, before artifact builds. It also checks packaging input
validation on both architectures.
Before uploading or
publishing, it tests the exported binary archive, installs and tests the exact
Debian package, and loads and tests the exported server and Home Assistant app
image archives. The image checks verify architecture, source revision, both
executable versions, license notices, HTTP readiness, shutdown and persistent
identity. Package smoke checks use no configured devices or cloud credentials.
The app's pinned server revision must contain the same Cargo/toolchain/runtime
inputs as the tagged source. Update its pin only after those inputs are committed.

Tagged runs use GitHub's built-in artifact attestations for packages and image
archives. Verify a downloaded package with `gh attestation verify --repo
mjc/gafctl PATH_TO_PACKAGE`; these attestations do not sign the GHCR manifest or
prove physical fan compatibility. Docker base images are pinned by immutable
multi-architecture index digest. Apt repositories still resolve packages at
build time, so byte-for-byte reproducibility is not claimed.
