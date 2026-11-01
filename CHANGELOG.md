# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). Until 1.0 the library schema may change
between minor versions; an older library is refused with a message rather than migrated.

## [Unreleased]

## [0.1.0] - 2026-11-01

First public release.

### Added

- One contract served three ways: MCP over stdio and streamable HTTP, REST under `/v1`, and a CLI.
- 43 built-in providers defined as TOML specs with recorded fixtures, plus `check_spec`, `save_spec` and
  `reset_spec` to validate and fix a provider whose API changed without a new release.
- A built-in `comfyui` provider for a self-hosted ComfyUI server.
- Jobs with a leased worker, retries and classified failures; batches; job chains with `{{job:<id>}}`
  placeholders; `cancel_job`.
- Reuse of identical generations instead of paying twice.
- Presets: a named provider, model and default input for a kind of task, so `generate {preset}` needs no
  price search; `search_prices` defaults to the providers the user has keys for.
- `keys set`, `keys list`, `keys test` and `keys remove`: keys are read from a hidden prompt or stdin, never
  through an agent, and stored in the OS keychain or a 0600 `config.toml`; a free `auth_check` per spec.
- Price catalog for video and image models, `estimate_cost`, offers and budgets with auto-estimates.
- Preflight checks and `doctor` before spending; a review gate and per-model stats.
- A local asset library on SQLite: references, collections, lineage, similarity search over embeddings,
  previews, collection export, ffprobe metadata and local ffmpeg edits.
- Provenance embedded in generated files and restored on import.
- Prompt blocks and recipes, served as MCP prompts; a recipe can hold a whole chain and run with one call.
- The agent skill `skills/ai-media-router/SKILL.md`, also served as the MCP resource `ai-media-router://guide`.
- `serve` refuses a non-loopback bind unless `AI_MEDIA_ROUTER_TOKEN` is set.
- CI on Linux, macOS and Windows with an end-to-end MCP check against emulated providers; release builds
  for five targets and a container image on GHCR.

[Unreleased]: https://github.com/jutsuhq/ai-media-router/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/jutsuhq/ai-media-router/releases/tag/v0.1.0
