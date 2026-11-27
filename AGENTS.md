# Agent & Contributor Guide

- Language: Rust (`cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`).
- Target OS: macOS (Apple Silicon & Intel, APFS).
- Keep the public repository clean: never commit personal machine names, local user home paths, or scratch benchmark dumps into tracked files.
- Privileged APFS integration tests (`diskutil apfs addVolume`/`deleteVolume`) must be gated behind `CAISSON_INTEGRATION_TESTS=1` and never run during default `cargo test`.
- Always parse `diskutil ... -plist` output structurally (via the `plist` crate) rather than scraping human text.
- Preserve safety invariants in `ARCHITECTURE.md` §6 unconditionally (`destroy` triple-check: registry UUID + `.caisson-volume.json` marker + non-system APFS role).
