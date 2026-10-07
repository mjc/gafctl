{
  lib,
  stdenv,
  makeRustPlatform,
  rustToolchain,
  pkg-config,
  cmake,
  dbus,
  libiconv,
  cacert,
  mosquitto,
}:
let
  rustPlatform = makeRustPlatform {
    cargo = rustToolchain;
    rustc = rustToolchain;
  };
  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../rust-toolchain.toml
      ../.clippy.toml
      ../LICENSE
      ../LICENSE-QUICKCONNECT-REFERENCE.txt
      ../THIRD-PARTY-NOTICES.txt
      ../LICENSE-RUST-STDLIB.html
      ../src
      ../tests
      ../fixtures
    ];
  };
in
rustPlatform.buildRustPackage {
  pname = "gafctl";
  version = (builtins.fromTOML (builtins.readFile ../Cargo.toml)).package.version;
  inherit src;
  cargoLock.lockFile = ../Cargo.lock;
  nativeBuildInputs = [
    pkg-config
    cmake
  ];
  nativeCheckInputs = [ mosquitto ];
  buildInputs =
    lib.optionals stdenv.hostPlatform.isLinux [ dbus ]
    ++ lib.optionals stdenv.hostPlatform.isDarwin [ libiconv ];
  SSL_CERT_FILE = "${cacert}/etc/ssl/certs/ca-bundle.crt";
  postInstall = ''
    install -Dm644 LICENSE "$out/share/licenses/gafctl/LICENSE"
    install -Dm644 LICENSE-QUICKCONNECT-REFERENCE.txt "$out/share/licenses/gafctl/LICENSE-QUICKCONNECT-REFERENCE.txt"
    install -Dm644 THIRD-PARTY-NOTICES.txt "$out/share/licenses/gafctl/THIRD-PARTY-NOTICES.txt"
    install -Dm644 LICENSE-RUST-STDLIB.html "$out/share/licenses/gafctl/LICENSE-RUST-STDLIB.html"
  '';
  meta = {
    description = "GAF attic fan control and Home Assistant integration";
    homepage = "https://github.com/mjc/gafctl";
    license = lib.licenses.mit;
    mainProgram = "gafctl";
    platforms = lib.platforms.linux ++ lib.platforms.darwin;
  };
}
