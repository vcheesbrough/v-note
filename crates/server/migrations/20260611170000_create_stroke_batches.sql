-- Canonical ink: append-only coalesced stroke batches per page.
-- `strokes` mirrors the wire batch (JSONB); `seq` is a per-page monotonic
-- sequence assigned by the server on commit. `client_batch_id` is a
-- client-generated idempotency key so reconnect retries do not double-insert.
CREATE TABLE stroke_batches (
    page_id TEXT NOT NULL REFERENCES pages (id) ON DELETE CASCADE,
    seq BIGINT NOT NULL,
    client_batch_id TEXT NOT NULL,
    strokes JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (page_id, seq)
);

-- Idempotency: a given client batch id commits at most once per page.
CREATE UNIQUE INDEX stroke_batches_client_batch_idx
    ON stroke_batches (page_id, client_batch_id);
