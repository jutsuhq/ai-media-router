# Contributing to AI Media Router

Thanks for helping. Bug reports, broken-provider reports, new provider specs and code changes are all welcome.
Coding agents should also read [`AGENTS.md`](AGENTS.md); this file follows it.

## Before you start

- For a bug, open an issue with the command or tool call, the provider and model, and the error text.
- For a provider whose API changed, use the **Provider broken / API changed** issue template. Often the fix is a
  spec edit, not Rust code.
- For anything larger than a fix, open an issue first so we can agree on the shape before you write it.

## Build and check

Rustup picks the pinned toolchain from `rust-toolchain.toml` automatically; the crate's minimum supported Rust is 1.89.
You also need Python 3 for the emulator.
`ffmpeg` is optional: tests that need it skip themselves when it is missing.

```bash
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --locked && python3 scripts/emulate.py --bin target/debug/ai-media-router
```

`scripts/emulate.py` (stdlib only) drives the real binary over MCP against emulated provider APIs and covers
every tool and every spec. CI runs all three on Linux; clippy and tests also run on macOS and Windows.

## Layout

```
src/
  main.rs            entry point
  app.rs, config.rs  shared state, config.toml [env]
  api/               the single contract: every operation and its types (they double as MCP schemas); tests.rs
  interfaces/        thin wrappers, no logic: cli.rs, mcp.rs (tools, prompts, resources), rest.rs (/v1)
  jobs/              lifecycle, leased worker, chains, retries, batches, placeholders, preflight, doctor
  library/           assets, references, collections, lineage, embeddings, previews, ffmpeg edits
  pricing/           price catalog, cost estimates, budgets
  templates/         snippets and recipes, MCP prompts
  providers/         TOML spec engine: spec.rs schema, engine.rs HTTP steps, fixtures.rs, auth.rs
  db/                connection setup (mod.rs) and the whole schema (schema.sql)
providers/*.toml, providers/fixtures/   built-in specs and recorded exchanges, embedded at build time
skills/ai-media-router/SKILL.md         the agent skill, also the MCP resource ai-media-router://guide
```

## Rules

- **New operation.** Add it to `src/api/mod.rs` first, then one line each in the `tools!` macro
  (`interfaces/mcp.rs`), the `handlers!` macro plus a route (`interfaces/rest.rs`), and a CLI command
  (`interfaces/cli.rs`). Tests go in `src/api/tests.rs`.
- **Schema.** `src/db/schema.sql` is the full schema, created on an empty database. The project is pre-release,
  so change it in place and bump `SCHEMA_VERSION`. An older library is refused with a message; it is not migrated.
- **Providers.** A provider is a TOML spec plus fixtures, not Rust code. [`docs/provider-spec.md`](docs/provider-spec.md)
  documents every field, and `check_spec` validates a spec and replays its fixtures. Never commit real API keys or
  unredacted responses in fixtures.
- **Model input** passes through unchanged. Do not add a unified input schema.
- **Errors** must say what to do next: name the provider, the step, the spec pointer and the fix.
- **Comments** at most three lines, and only for what the code cannot say. History goes in the commit message.

## Pull requests

- Keep a PR to one change, with a commit message that says what changed and why.
- Run the three checks above before pushing.
- Update `README.md`, `docs/` or `skills/ai-media-router/SKILL.md` when behaviour an agent or user relies on changes,
  and add a line under **Unreleased** in [`CHANGELOG.md`](CHANGELOG.md).

By contributing you agree that your work is released under the [MIT License](LICENSE) and that you follow the
[Code of Conduct](CODE_OF_CONDUCT.md).
