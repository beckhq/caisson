# Contributing to Caisson

Thank you for your interest in contributing to **Caisson**!

## Development Requirements

- **OS:** macOS (Apple Silicon or Intel) on APFS
- **Toolchain:** Stable Rust (`rustfmt` and `clippy` components installed)

## Verification Before Opening a PR

All pull requests must pass the full unprivileged check suite:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

### Privileged APFS Integration Tests

Tests that invoke `diskutil apfs addVolume` / `deleteVolume` are strictly gated behind `CAISSON_INTEGRATION_TESTS=1` and never run during default `cargo test`:

```bash
sudo CAISSON_INTEGRATION_TESTS=1 cargo test --test apfs_privileged -- --nocapture
```

## Engineering Principles

1. **Conventional Commits:** Prefix commits and PR titles with `feat:`, `fix:`, `docs:`, `test:`, `refactor:`, `perf:`, `ci:`, or `chore:`. Squash-merge PRs into `main` so git history remains clean and release notes auto-generate cleanly.
2. **Structured `diskutil` Parsing:** Always parse `diskutil ... -plist` output structurally via the `plist` crate — never scrape human-readable text output.
3. **Unconditional Safety Invariants:** Never weaken the safety invariants documented in [`ARCHITECTURE.md`](./ARCHITECTURE.md) §6:
   - Triple-checked `destroy` (registry UUID + `.caisson-volume.json` marker UUID + non-system APFS role guard + explicit confirmation/`--yes`).
   - Strict volume naming (`[a-z0-9][a-z0-9._-]*`).
   - Strict privilege boundary (zero `sudo` for day-to-day worktree operations, zero persistent root daemon).
4. **Repository Hygiene:** Never commit local user home paths, personal hostnames, or raw machine dumps.
