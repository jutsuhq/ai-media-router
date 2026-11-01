---
name: ai-media-router
description: Generate images and video through the user's own provider API keys with AI Media Router (MCP server `ai-media-router` or the `ai-media-router` CLI), keep results in a local asset library, reuse characters and styles, run batches and chains, and stay within a budget. Use when the user asks to make, vary, extend or edit images or video clips, compare models or prices, or fix a provider integration.
---

# AI Media Router

A local gateway: the user's keys, 40+ providers, one contract. Every result is downloaded into a
local library with its prompt, cost and lineage. The MCP tools and the CLI do the same things.

## First contact

1. `doctor` checks the setup: data directory, database, ffmpeg, which providers have keys, specs.
   A `fail` names what to fix.
2. `list_keys` shows each key variable, which providers use it, whether it is set, its source and its
   masked tail. When one is missing, give the user its `set_command` (`ai-media-router keys set FAL_KEY`)
   to run in their terminal; never ask for a key value in the chat. Keys apply without a restart.
   `test_key {provider}` makes a free auth check where the spec has one.
3. `list_providers` shows which providers are configured.

## Generate one thing

1. `list_recipes` first. If a single-generation preset fits (`video-draft`, `video-final`, `hero-image`, `image-edit`…),
   call `generate {recipe, input}` and skip price search: the preset supplies provider, model, input,
   save_to, offer_id and budget, and your `input` merges over its input field by field (the call wins).
   `vars` fills `{{var:…}}` as in `run_recipe`. A recipe with `steps` is a chain: use `run_recipe`.
2. Only when choosing: setting up a new preset, or several of the user's providers serve the same model.
   Then `search_prices {query, res}` → pick an offer; note `provider_key` and `offer_id`. It defaults to
   the user's configured providers once any key is set (`configured_only: false` for all). Offer to save
   the choice with `save_recipe` (with `offer_id`).
3. `get_provider {provider}` → `model_hint` and `example` show what `model` and `input` mean there.
   `input` goes to the provider unchanged, so use that model's own field names.
4. `preflight` with the exact arguments you plan for `generate`. It sends nothing and spends nothing,
   and checks keys, placeholders, upload support, cost and budget. It also renders the HTTP requests.
   Do this before anything expensive or on a provider you have not used in this session.
5. `generate {provider, model, input, offer_id, save_to}` → returns the job immediately. `provider` and
   `model` are required only without `recipe`.
6. `get_job {job_id, wait_secs: 50}` until `status` is `done` or `failed`. Outputs are in `assets[].path`.

## Rules

- **Money.** For paid runs pass `offer_id`, so the job carries an estimate. When the user names a limit,
  `set_budget {name, limit_usd}` once, then pass `budget` on every job; over-limit jobs are refused before submit.
- **Failures.** A failed job has `error_kind`, `retryable` and a `hint`. Follow the hint. Never resubmit
  `moderation`, `auth`, `quota` or `invalid_input` unchanged. On `auth`, call `list_keys` and give the user
  the `set_command` to run in their terminal; never ask for a key value in the chat.
- **Flaky providers.** `retry: {attempts: 1-5, delay_secs}` on generate, run_recipe or batch defaults resubmits
  after `rate_limit`, `provider_error`, `network` or `timeout`. It is off by default because a provider may bill
  a failed attempt, so turn it on for cheap or unattended runs.
- **Repeats.** `reuse: true` on generate, run_recipe or batch defaults returns an identical finished job's outputs
  at no cost (`reused_from` names it). Use it when re-running a pipeline whose steps did not change; leave it off
  when the user wants a new variant. Calling generate again while the identical first call still runs returns
  that job (`note` says so); pass `reuse: false` when you really want a second one.
- **Choosing a model.** `get_stats {provider?, model?}` shows what worked before: done share, failure kinds,
  ratings, cost, seconds.
- Do not invent model ids. Take them from `get_provider`, its docs link, or a past job (`list_jobs`).

## Library files in input

Put a placeholder as a whole string value. It is uploaded through the provider (cached) before submit:

| Placeholder | Becomes |
|---|---|
| `"{{asset:<id>}}"` | that asset |
| `"{{ref:<name>}}"` | a reference's assets: all of them in an array, else the first |
| `"{{job:<id>}}"` / `"{{job:<id>:<n>}}"` | another job's outputs; the new job waits while that one runs |
| `"{{job:<id>:last_frame}}"` / `first_frame` | a frame of that job's video, for the next shot |

Inside any string: `{{snippet:<name>}}`, `{{ref:<name>.description}}`, `{{ref:<name>.tags}}`.

## Consistency across shots

`save_reference {name, kind: character|style|product|…, description, asset_ids}` once, then use
`"{{ref:<name>}}"` as the image input and `{{ref:<name>.description}}` in the prompt.
`save_to {reference, collection}` files new outputs back into it.

## Many variants

`generate_batch {defaults, items, matrix, label}` runs items × matrix combinations, at most 100 jobs.
A budget admits the whole batch at once. Follow it with `get_batch {batch_id, wait_secs}`.
`generate_batch {recipe, matrix: {<variable>: [...]}}` runs a saved recipe for every combination.

## Keep what worked

- **Presets.** `save_recipe {name, provider, model, input, save_to, offer_id, budget, description}` saves one
  generation as a named preset; `generate {recipe, input}` runs it. `list_recipes` shows them.
- `save_recipe {name, from_job}` stores a working generation as a recipe, with `{{var:<name>}}` marking what
  changes. `run_recipe {name, vars}` reruns it.
- A whole chain is one recipe: `save_recipe {name, from_job: <final job>, chain: true}` saves the jobs behind it
  as steps, or write `steps: [{name, provider, model, input} | {name, op: "trim"|"concat"|"mux_audio"|…, input}]`
  with `{{step:<name>}}` (`:<n>`, `:last_frame`) for an earlier step's outputs. Price each generation step with
  `estimated_cost_usd` or `offer_id`; `preflight {recipe, vars, budget}` checks the whole run first.
  `run_recipe` starts every step at once and returns the last step's job: follow it with `get_batch {batch_id}`,
  whose `outputs` are the final files. A failed step fails the steps after it, naming it.
- `review_asset {asset_ids, status: approved|rejected, rating}`. A rejected asset is skipped by `{{ref:…}}`, and
  `export_collection {only_approved}` ships only the approved ones.
- Generated PNG, JPEG and MP4/MOV/WebM files carry their provider, model, input, job and parents. `import_asset`
  of such a file restores prompt and lineage; then `save_recipe {name, from_job: <its job_id>}` reuses it.

## Local edits (ffmpeg)

`extract_frame`, `reframe`, `trim`, `concat`, `overlay`, `mux_audio`. Each makes a new asset, recorded in lineage.

## A provider broke

`get_job {job_id, debug: true}` (raw exchanges) → `get_spec` → edit the TOML → `check_spec {spec, model, input}`
→ `save_spec`. The change is active at once, with no rebuild. `reset_spec` returns to the built-in spec.

## CLI equivalents

`ai-media-router doctor`, `prices <query> [--all]`, `generate --recipe <name> --input '{…}'` or
`generate --provider … --model … --input '{…}' [--dry-run] [--retry 2] [--reuse] --wait`,
`jobs [id] --wait`, `batch @spec.json --wait`, `recipes save <name> --provider … --model … --input '{…}' [--offer …] [--budget …]`,
`recipes run <name> --vars '{…}'` (`recipes check` dry-runs it),
`keys set|list|test|remove`. Every command has `--help`.
