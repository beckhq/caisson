## Summary
- What changed and why?

## Verification
- [ ] `cargo fmt --check`
- [ ] `cargo clippy --all-targets -- -D warnings`
- [ ] `cargo test`
- [ ] Safety invariants in `ARCHITECTURE.md` §6 preserved (`destroy` triple-check, no spaces in volume names, privilege boundaries)
- [ ] Any privileged APFS tests remain gated behind `CAISSON_INTEGRATION_TESTS=1`
