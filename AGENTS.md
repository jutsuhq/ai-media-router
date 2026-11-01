# Working on AI Media Router

Notes for coding agents and contributors. To *use* the router, read `skills/ai-media-router/SKILL.md`,
which is also served as the MCP resource `ai-media-router://guide`.

## Build and check

```bash
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --locked && python3 scripts/emulate.py --bin target/debug/ai-media-router
```

`scripts/emulate.py` (stdlib only) drives the real binary over MCP against emulated provider APIs. It
covers every tool and every spec. CI runs all three. Tests that need ffmpeg skip themselves when it is missing.

## Layout

```
src/
  main.rs            entry point
  app.rs, config.rs  shared state, config.toml [env]
  api/               the single contract: every operation and its types (they double as MCP schemas); tests.rs
  interfaces/        thin wrappers, no logic: cli.rs, mcp.rs (tools, prompts, resources), rest.rs (/v1)
  jobs/              lifecycle, leased worker, chains, retries (mod.rs); batches, placeholders,
                     preflight + doctor, failure kinds, per-model stats
  library/           assets, references, collections, lineage, embeddings, previews, ffmpeg edits
  pricing/           price catalog (video + image), cost estimates, budgets
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
- **Schema.** `src/db/schema.sql` is the full schema, created on an empty database. The app is pre-release,
  so change it in place and bump `SCHEMA_VERSION`. An older library is refused with a message; it is not migrated.
- **Providers.** A provider is a TOML spec plus fixtures, not Rust code. `docs/provider-spec.md` documents
  every field, and `check_spec` validates a spec and replays its fixtures.
- **Model input** passes through unchanged. Do not add a unified input schema.
- **Errors** must say what to do next: name the provider, the step, the spec pointer and the fix.
- **Comments** at most three lines, and only for what the code cannot say. History goes in the commit message.
