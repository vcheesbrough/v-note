-- Protocol v3 makes stroke identity and immutable style canonical. Existing
-- development rows are backfilled deterministically from page/batch/ordinal.
ALTER TABLE pages ADD COLUMN IF NOT EXISTS ink_revision BIGINT NOT NULL DEFAULT 0;

UPDATE pages p
SET ink_revision = COALESCE((SELECT MAX(seq) FROM stroke_batches b WHERE b.page_id = p.id), 0)
WHERE ink_revision = 0;

CREATE TABLE stroke_tombstones (
    page_id TEXT NOT NULL REFERENCES pages (id) ON DELETE CASCADE,
    stroke_id TEXT NOT NULL,
    deleted_revision BIGINT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (page_id, stroke_id)
);

CREATE TABLE tombstone_batches (
    page_id TEXT NOT NULL REFERENCES pages (id) ON DELETE CASCADE,
    client_mutation_id TEXT NOT NULL,
    revision BIGINT NOT NULL,
    stroke_ids JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (page_id, client_mutation_id)
);
