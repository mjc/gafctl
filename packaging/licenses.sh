#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
case "${1:---check}" in
    --check|--update) ;;
    *) echo 'Use --check or --update' >&2; exit 1 ;;
esac
work="$PWD/target/licenses"
mkdir -p "$work"
python3 - "$PWD" "$work" <<'PY'
import hashlib
import json
import subprocess
import sys
import tomllib
from pathlib import Path

root, work = map(Path, sys.argv[1:])
locked = {
    (package["name"], package["version"])
    for package in tomllib.loads((root / "Cargo.lock").read_text())["package"]
}
rust_version = tomllib.loads((root / "rust-toolchain.toml").read_text())["toolchain"]["channel"]
if subprocess.check_output(["rustc", "--version"], text=True).split()[1] != rust_version:
    raise SystemExit("Use the repository's pinned Rust toolchain")
for source in json.loads((root / "packaging/license-reference/sources.json").read_text()):
    if source.get("rust_version", rust_version) != rust_version:
        raise SystemExit("Refresh the cached Rust license for this toolchain")
    if any((package["name"], package["version"]) not in locked for package in source.get("packages", [])):
        raise SystemExit(f"Refresh the cached package license: {source['file']}")
    contents = (root / "packaging/license-reference" / source["file"]).read_bytes()
    if hashlib.sha256(contents).hexdigest() != source["sha256"]:
        raise SystemExit(f"Cached license checksum mismatch: {source['file']}")
reference = json.dumps(str(root / "packaging/license-reference"), ensure_ascii=False)[1:-1]
config = (root / "about.toml").read_text().replace("@GAFCTL_LICENSE_REFERENCE@", reference)
(work / "about.toml").write_text(config)
PY
cargo fetch --locked
cargo about generate --locked --offline --all-features --fail --config "$work/about.toml" --format json --output-file "$work/dependencies.json"
jq -e --slurpfile sources packaging/license-reference/sources.json '
    . as $report | all($sources[0][];
        all(.packages[]?; . as $source |
            [$report.crates[].package | select(.name == $source.name) | .version] as $active |
            ($active | length) > 0 and all($active[]; . == $source.version)))
' "$work/dependencies.json" >/dev/null
jq -e '(.licenses | length) > 0 and all(.licenses[]; .source_path != null)' "$work/dependencies.json" >/dev/null
jq -r '"Third-party dependency notices\n", (.licenses | group_by(.text)[] | "============================================================", (map("\(.name) (\(.id))") | unique | join(" / ")), "", (map(.used_by[].crate | "\(.name) \(.version)") | unique | join("\n")), "", .[0].text, "")' "$work/dependencies.json" > "$work/THIRD-PARTY-NOTICES.txt"
rust_sysroot=$(rustc --print sysroot)
printf '\n============================================================\nRust standard library MIT license\n\n' >> "$work/THIRD-PARTY-NOTICES.txt"
cat packaging/license-reference/rust-1.98.1-LICENSE-MIT >> "$work/THIRD-PARTY-NOTICES.txt"
for source in \
    lib/rustlib/src/rust/library/compiler-builtins/LICENSE.txt \
    lib/rustlib/src/rust/src/llvm-project/libunwind/LICENSE.TXT; do
    printf '\n============================================================\nRust runtime: %s\n\n' "$source" >> "$work/THIRD-PARTY-NOTICES.txt"
    cat "$rust_sysroot/$source" >> "$work/THIRD-PARTY-NOTICES.txt"
done
cp -f "$rust_sysroot/share/doc/rust/COPYRIGHT-library.html" "$work/LICENSE-RUST-STDLIB.html"
for name in THIRD-PARTY-NOTICES.txt LICENSE-RUST-STDLIB.html; do
    case "${1:---check}" in
        --check) cmp "$work/$name" "$name" ;;
        --update) cp -f "$work/$name" "$name" ;;
    esac
done
