-- Thumbnail artifacts are immutable per ink revision, so every persisted add
-- must retain the page revision at which it became visible. Tombstone batches
-- already store their revisions; existing adds occupy the remaining revision
-- positions in stroke sequence order.
ALTER TABLE stroke_batches ADD COLUMN IF NOT EXISTS revision BIGINT;

WITH available_revisions AS (
    SELECT
        page.id AS page_id,
        candidate.revision,
        row_number() OVER (
            PARTITION BY page.id
            ORDER BY candidate.revision
        ) AS stroke_seq
    FROM pages AS page
    CROSS JOIN LATERAL generate_series(1, page.ink_revision) AS candidate(revision)
    WHERE NOT EXISTS (
        SELECT 1
        FROM tombstone_batches AS tombstone
        WHERE tombstone.page_id = page.id
          AND tombstone.revision = candidate.revision
          AND jsonb_array_length(tombstone.stroke_ids) > 0
    )
)
UPDATE stroke_batches AS batch
SET revision = available.revision
FROM available_revisions AS available
WHERE available.page_id = batch.page_id
  AND available.stroke_seq = batch.seq
  AND batch.revision IS NULL;

DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM stroke_batches WHERE revision IS NULL) THEN
        RAISE EXCEPTION 'could not reconstruct every stroke batch revision';
    END IF;
END
$$;

ALTER TABLE stroke_batches ALTER COLUMN revision SET NOT NULL;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM pg_constraint
        WHERE conname = 'stroke_batches_revision_positive'
          AND conrelid = 'stroke_batches'::regclass
    ) THEN
        ALTER TABLE stroke_batches
            ADD CONSTRAINT stroke_batches_revision_positive CHECK (revision > 0);
    END IF;
END
$$;

CREATE UNIQUE INDEX IF NOT EXISTS stroke_batches_page_revision_idx
    ON stroke_batches (page_id, revision);
