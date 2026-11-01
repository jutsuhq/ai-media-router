## What and why

<!-- One change per PR. Link the issue if there is one. -->

## Checks

- [ ] `cargo clippy --all-targets --locked -- -D warnings`
- [ ] `cargo test --locked`
- [ ] `cargo build --locked && python3 scripts/emulate.py --bin target/debug/ai-media-router`
- [ ] New operations are wired into `api/mod.rs`, `tools!`, `handlers!` + route, and the CLI
- [ ] Provider changes are a spec + fixtures, with no keys in fixtures
- [ ] README / `docs/` / `SKILL.md` and `CHANGELOG.md` updated where behaviour changed
