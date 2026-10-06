# Cached upstream license references

This directory stores upstream notices missing from published crate packages
and the pinned Rust MIT license. Preserve the source bytes. `about.toml` gives
cargo-about their SHA-256 checksums. The generation script prepares local paths,
checks package versions and formats cargo-about’s output.

`sources.json` records the exact published package versions, upstream commit URLs,
and SHA-256 checksums. The upstream commits came from each crate’s packaged
.cargo_vcs_info.json; Rust’s commit is the resolved `1.98.1` release tag. A crate or
toolchain update requires reviewing and refreshing its references.

## License sources

- BlueZ crates use their upstream MIT license with the original Luis Félix notice.
- rumqtt crates use the full upstream Apache 2.0 license.
- objc2-family upstream trees supply `LICENSE.md`, which declares
  MIT licensing and links to the terms. The consolidated notice includes that
  file and full MIT terms from the other notices and pinned Rust license. Keep
  upstream copyright text; Cargo `authors` are not copyright notices.
- btleplug uses its packaged BSD-3-Clause license, one of its declared choices.
- dunce uses its packaged CC0 license, one of its declared choices.
- AWS-LC clarifications use the full packaged composite notice and the additional
  packaged fiat-crypto MIT notice.
- Every generated entry requires a cargo-about `source_path`. Review each
  clarification source. The objc2 policy files link to terms, so
  the consolidated notice also needs the full terms.

## Provenance

| File | Applies to | SHA-256 | Pinned public source |
|---|---|---|---|
| block2-0.6.2-LICENSE.md | block2 0.6.2 | `7f976f7e9cb2d87df7230606feb932c3f21ac0e664045a775b600046ff850c54` | [upstream](https://raw.githubusercontent.com/madsmtm/objc2/b4167b582b2f75f9a1be75495c41b765344fd03c/LICENSE.md) |
| objc2-0.6.4-LICENSE.md | objc2 0.6.4 | `7f976f7e9cb2d87df7230606feb932c3f21ac0e664045a775b600046ff850c54` | [upstream](https://raw.githubusercontent.com/madsmtm/objc2/8852b424193ca41602281b3d7540d7c8ed51e49a/LICENSE.md) |
| objc2-encode-4.1.0-LICENSE.md | objc2-encode 4.1.0 | `7f976f7e9cb2d87df7230606feb932c3f21ac0e664045a775b600046ff850c54` | [upstream](https://raw.githubusercontent.com/madsmtm/objc2/8d214f5477365ffcbcbb7de058c86ed9a518efb7/LICENSE.md) |
| objc2-frameworks-0.3.2-LICENSE.md | objc2-core-bluetooth 0.3.2, objc2-foundation 0.3.2 | `7f976f7e9cb2d87df7230606feb932c3f21ac0e664045a775b600046ff850c54` | [upstream](https://raw.githubusercontent.com/madsmtm/objc2/7b1abfd750a2cacaea71d6a56ecfb83cb7de560b/LICENSE.md) |
| bluez-async-0.8.2-LICENSE-MIT | bluez-async 0.8.2 | `eef0c0b2cf6e14147a8362cfa2408a79220cc1ea22bab1a2e188b9cd1d2e0f0d` | [upstream](https://raw.githubusercontent.com/bluez-rs/bluez-async/6c0204b0e28804cc8b4937eec43cba9c72e0bfd5/LICENSE-MIT) |
| bluez-generated-0.4.0-LICENSE-MIT | bluez-generated 0.4.0 | `eef0c0b2cf6e14147a8362cfa2408a79220cc1ea22bab1a2e188b9cd1d2e0f0d` | [upstream](https://raw.githubusercontent.com/bluez-rs/bluez-async/0162fe1f036e7ceef218db49e8d034692b80f8ca/LICENSE-MIT) |
| rumqtt-next-0.34.0-LICENSE | mqttbytes-core-next 0.34.0, rumqttc-core-next 0.34.0, rumqttc-next 0.34.0, rumqttc-v5-next 0.34.0 | `cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30` | [upstream](https://raw.githubusercontent.com/thehouseisonfire/rumqtt/aa7a694f9b76b17d4c31200cf73d79616acae9b3/LICENSE) |
| rust-1.98.1-LICENSE-MIT | Rust 1.98.1 | `b71bd43a069ca0641a9ecfe585ca7b3c53b5cc1608f8b68321168698e28b5ea1` | [upstream](https://raw.githubusercontent.com/rust-lang/rust/48a229ceaefd4985c50990b14116b6d856af0985/LICENSE-MIT) |
