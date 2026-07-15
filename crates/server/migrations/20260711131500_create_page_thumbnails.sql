CREATE TABLE page_thumbnails (
    page_id TEXT NOT NULL REFERENCES pages (id) ON DELETE CASCADE,
    source_seq BIGINT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('generating', 'available', 'failed')),
    png BYTEA,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (page_id, source_seq),
    CHECK ((status = 'available') = (png IS NOT NULL))
);

CREATE INDEX page_thumbnails_retention_idx
    ON page_thumbnails (page_id, created_at DESC);
