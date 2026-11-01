-- The whole schema, created once on an empty database. Bump SCHEMA_VERSION in mod.rs when it changes.
-- Link tables read in insertion order (job_inputs, job_outputs) keep their rowid; the others are WITHOUT ROWID.

CREATE TABLE batches (
    id         TEXT PRIMARY KEY,
    label      TEXT,
    -- recipe name when the batch is one run_recipe of a multi-step recipe
    recipe     TEXT,
    created_at INTEGER NOT NULL
) STRICT;

-- spend caps; a job counts its reported USD cost, else the estimate given at generate
CREATE TABLE budgets (
    name       TEXT PRIMARY KEY COLLATE NOCASE,
    limit_usd  REAL NOT NULL,
    created_at INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

CREATE TABLE jobs (
    id           TEXT PRIMARY KEY,
    provider     TEXT NOT NULL,
    model        TEXT NOT NULL,
    input        TEXT NOT NULL,
    remote_id    TEXT NOT NULL,
    -- variables captured by the provider spec (task id, status URLs, ...), see providers/engine.rs
    vars         TEXT NOT NULL DEFAULT '{}',
    status       TEXT NOT NULL CHECK (status IN ('running', 'done', 'failed')),
    error        TEXT,
    -- failures.rs kind of a failed job
    error_kind   TEXT,
    cost         REAL,
    cost_unit    TEXT,
    -- last request/response per step (providers/exchange.rs)
    trace        TEXT NOT NULL DEFAULT '[]',
    -- what the provider last said while running, or why the last poll or attempt failed
    progress     TEXT,
    poll_errors  INTEGER NOT NULL DEFAULT 0,
    -- next poll time; also the cross-process claim, since every stdio MCP process runs a worker
    lease_until  INTEGER NOT NULL DEFAULT 0,
    -- {collection, reference (ref id), role}: where finished outputs are filed
    save_to      TEXT,
    budget       TEXT REFERENCES budgets (name) ON DELETE SET NULL,
    estimate_usd REAL,
    -- JSON list of parent job ids; while non-empty the job is `waiting`. '[]' queues a submit (retry)
    waiting_on   TEXT,
    batch_id     TEXT REFERENCES batches (id) ON DELETE SET NULL,
    -- recipe step name within the batch
    step         TEXT,
    retries_left INTEGER NOT NULL DEFAULT 0,
    retry_delay  INTEGER NOT NULL DEFAULT 30,
    attempts     INTEGER NOT NULL DEFAULT 1,
    -- sha256 of provider + model + input with library files as asset ids (jobs/reuse.rs)
    fingerprint  TEXT,
    -- job whose outputs this one returns instead of generating (reuse); while it runs, waiting_on names it
    reused_from  TEXT REFERENCES jobs (id) ON DELETE SET NULL,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL
) STRICT;
CREATE INDEX jobs_due ON jobs (lease_until) WHERE status = 'running';
CREATE INDEX jobs_created ON jobs (created_at);
CREATE INDEX jobs_model ON jobs (provider, model);
CREATE INDEX jobs_failed ON jobs (error_kind, updated_at) WHERE status = 'failed';
CREATE INDEX jobs_batch ON jobs (batch_id) WHERE batch_id IS NOT NULL;
CREATE INDEX jobs_budget ON jobs (budget) WHERE budget IS NOT NULL;
CREATE INDEX jobs_fingerprint ON jobs (fingerprint, created_at) WHERE fingerprint IS NOT NULL AND reused_from IS NULL;

CREATE TABLE assets (
    id            TEXT PRIMARY KEY,
    file          TEXT NOT NULL,
    mime          TEXT,
    bytes         INTEGER NOT NULL,
    -- first job that made the file; job_outputs has every one
    job_id        TEXT REFERENCES jobs (id),
    source_url    TEXT,
    prompt        TEXT,
    tags          TEXT NOT NULL DEFAULT '',
    description   TEXT NOT NULL DEFAULT '',
    status        TEXT NOT NULL DEFAULT 'draft' CHECK (status IN ('draft', 'approved', 'rejected')),
    rating        INTEGER CHECK (rating BETWEEN 1 AND 5),
    review_notes  TEXT NOT NULL DEFAULT '',
    -- ffprobe metadata (media.rs); has_audio stays NULL until the file was probed
    width         INTEGER,
    height        INTEGER,
    duration_secs REAL,
    has_audio     INTEGER,
    created_at    INTEGER NOT NULL
) STRICT;
CREATE INDEX assets_job ON assets (job_id) WHERE job_id IS NOT NULL;
CREATE INDEX assets_created ON assets (created_at);
CREATE INDEX assets_status ON assets (status);

CREATE VIRTUAL TABLE assets_fts USING fts5 (prompt, tags, description, review_notes, content = 'assets');
CREATE TRIGGER assets_ai AFTER INSERT ON assets BEGIN
    INSERT INTO assets_fts (rowid, prompt, tags, description, review_notes)
    VALUES (new.rowid, new.prompt, new.tags, new.description, new.review_notes);
END;
CREATE TRIGGER assets_ad AFTER DELETE ON assets BEGIN
    INSERT INTO assets_fts (assets_fts, rowid, prompt, tags, description, review_notes)
    VALUES ('delete', old.rowid, old.prompt, old.tags, old.description, old.review_notes);
END;
CREATE TRIGGER assets_au AFTER UPDATE OF prompt, tags, description, review_notes ON assets BEGIN
    INSERT INTO assets_fts (assets_fts, rowid, prompt, tags, description, review_notes)
    VALUES ('delete', old.rowid, old.prompt, old.tags, old.description, old.review_notes);
    INSERT INTO assets_fts (rowid, prompt, tags, description, review_notes)
    VALUES (new.rowid, new.prompt, new.tags, new.description, new.review_notes);
END;

-- `references` is an SQL keyword, hence refs
CREATE TABLE refs (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE COLLATE NOCASE,
    kind        TEXT NOT NULL CHECK (kind IN ('character', 'style', 'product', 'environment', 'composition', 'brand')),
    description TEXT NOT NULL DEFAULT '',
    tags        TEXT NOT NULL DEFAULT '',
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
) STRICT;
CREATE TABLE ref_assets (
    ref_id   TEXT NOT NULL REFERENCES refs (id) ON DELETE CASCADE,
    asset_id TEXT NOT NULL REFERENCES assets (id) ON DELETE CASCADE,
    role     TEXT,
    position INTEGER NOT NULL,
    PRIMARY KEY (ref_id, asset_id)
) STRICT, WITHOUT ROWID;
CREATE INDEX ref_assets_asset ON ref_assets (asset_id);

CREATE TABLE collections (
    name        TEXT PRIMARY KEY COLLATE NOCASE,
    description TEXT NOT NULL DEFAULT '',
    created_at  INTEGER NOT NULL
) STRICT, WITHOUT ROWID;
CREATE TABLE collection_assets (
    collection TEXT NOT NULL REFERENCES collections (name) ON DELETE CASCADE,
    asset_id   TEXT NOT NULL REFERENCES assets (id) ON DELETE CASCADE,
    added_at   INTEGER NOT NULL,
    PRIMARY KEY (collection, asset_id)
) STRICT, WITHOUT ROWID;
CREATE INDEX collection_assets_asset ON collection_assets (asset_id);

-- lineage edges; asset_id has no foreign key so history survives deleting the asset
CREATE TABLE job_inputs (
    job_id   TEXT NOT NULL REFERENCES jobs (id) ON DELETE CASCADE,
    asset_id TEXT NOT NULL,
    ref_id   TEXT REFERENCES refs (id) ON DELETE SET NULL,
    PRIMARY KEY (job_id, asset_id)
) STRICT;
CREATE INDEX job_inputs_asset ON job_inputs (asset_id);
CREATE INDEX job_inputs_ref ON job_inputs (ref_id) WHERE ref_id IS NOT NULL;

-- every job that produced an asset; identical files from several jobs share one asset row
CREATE TABLE job_outputs (
    job_id   TEXT NOT NULL REFERENCES jobs (id) ON DELETE CASCADE,
    asset_id TEXT NOT NULL,
    PRIMARY KEY (job_id, asset_id)
) STRICT;
CREATE INDEX job_outputs_asset ON job_outputs (asset_id);

-- what an asset uploaded to a provider resolves to (URL, file id), reused until it expires
CREATE TABLE uploads (
    asset_id   TEXT NOT NULL REFERENCES assets (id) ON DELETE CASCADE,
    provider   TEXT NOT NULL,
    reference  TEXT NOT NULL,
    expires_at INTEGER NOT NULL,
    PRIMARY KEY (asset_id, provider)
) STRICT, WITHOUT ROWID;

-- text embeddings of prompt + tags + description; `hash` detects edited text
CREATE TABLE embeddings (
    asset_id TEXT PRIMARY KEY REFERENCES assets (id) ON DELETE CASCADE,
    model    TEXT NOT NULL,
    hash     TEXT NOT NULL,
    vector   BLOB NOT NULL
) STRICT;
CREATE INDEX embeddings_model ON embeddings (model);

-- reusable prompt blocks ({{snippet:<name>}}) and saved generation templates (run_recipe)
CREATE TABLE snippets (
    name        TEXT PRIMARY KEY COLLATE NOCASE,
    text        TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
) STRICT, WITHOUT ROWID;
CREATE TABLE recipes (
    name        TEXT PRIMARY KEY COLLATE NOCASE,
    description TEXT NOT NULL DEFAULT '',
    -- provider, model and input (JSON) of a single-generation recipe; NULL when it has steps
    provider    TEXT,
    model       TEXT,
    input       TEXT,
    -- JSON list of steps (recipes.rs Step), '[]' for a single generation
    steps       TEXT NOT NULL DEFAULT '[]',
    save_to     TEXT,
    -- defaults of a single-generation recipe
    offer_id    TEXT,
    budget      TEXT,
    variables   TEXT NOT NULL DEFAULT '{}',
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
) STRICT, WITHOUT ROWID;
