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
      ../src
      ../crates
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
  '';
  meta = {
    description = "GAF attic fan control and Home Assistant integration";
    homepage = "https://github.com/mjc/gafctl";
    license = lib.licenses.mit;
    mainProgram = "gafctl";
    platforms = lib.platforms.linux ++ lib.platforms.darwin;
  };
}
