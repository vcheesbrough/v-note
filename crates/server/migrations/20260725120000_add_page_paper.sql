-- Per-page paper (rule lines). The first client-settable page attribute beyond
-- the write-once title. Wire values are stored verbatim, so `protocol::Paper`'s
-- `from_wire`/`wire_value` is the only mapping in the system (the
-- `page_thumbnails.status` → `ThumbnailMetadata` precedent).
--
-- Idempotent and backfill-free: every existing page keeps rendering exactly as
-- it does today, because 'none' is a pure no-op in all three renderers.
ALTER TABLE pages ADD COLUMN IF NOT EXISTS paper TEXT NOT NULL DEFAULT 'none';

-- Thumbnails are immutable per revision, so each JOB records the paper in force
-- at THAT revision. Reading pages.paper at render time would rasterize a later
-- paper into an already-published source_seq — and `recover_pending` re-queues
-- jobs after a restart, by which point the paper passed in memory to `enqueue`
-- is gone. The job row is the only durable source; never fall back to
-- pages.paper here.
ALTER TABLE page_thumbnails ADD COLUMN IF NOT EXISTS paper TEXT NOT NULL DEFAULT 'none';

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM pg_constraint
        WHERE conname = 'pages_paper_known'
          AND conrelid = 'pages'::regclass
    ) THEN
        ALTER TABLE pages
            ADD CONSTRAINT pages_paper_known CHECK (paper IN (
                'none',
                'ruled-margin-narrow',
                'ruled-margin-wide',
                'ruled-narrow',
                'ruled-wide',
                'squared-small',
                'squared-large'
            ));
    END IF;

    IF NOT EXISTS (
        SELECT 1
        FROM pg_constraint
        WHERE conname = 'page_thumbnails_paper_known'
          AND conrelid = 'page_thumbnails'::regclass
    ) THEN
        ALTER TABLE page_thumbnails
            ADD CONSTRAINT page_thumbnails_paper_known CHECK (paper IN (
                'none',
                'ruled-margin-narrow',
                'ruled-margin-wide',
                'ruled-narrow',
                'ruled-wide',
                'squared-small',
                'squared-large'
            ));
    END IF;
END
$$;
