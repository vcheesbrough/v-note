-- Recycle bin for pages (#631). A page now has three states:
--
--   live                 binned_at IS NULL
--   in the bin           binned_at IS NOT NULL — hidden from the library, ink
--                        and everything derived from it kept, commits accepted
--   permanently deleted  the row is gone (cascading to its ink, tombstones and
--                        thumbnails) and its id is recorded in purged_pages
--
-- Expand-only and compatible with the previous release: a nullable column every
-- existing page reads as live, and a table it never touches. A 0.79 server
-- running against this schema during a deploy still lists, edits and
-- hard-deletes pages exactly as before.
ALTER TABLE pages ADD COLUMN IF NOT EXISTS binned_at TIMESTAMPTZ;

-- The bin listing: an owner's binned pages, most recently binned first.
CREATE INDEX IF NOT EXISTS pages_owner_binned_at_idx
    ON pages (owner_id, binned_at DESC)
    WHERE binned_at IS NOT NULL;

-- The ids of permanently deleted pages, so a write for one is answered with
-- its own definitive class (`page_deleted`, HTTP 410) instead of the
-- not-found-or-not-yours 403. Offline replay (#159) relies on that answer to
-- offer "keep these strokes as a new page" — the device holds the only copy.
-- Holds no page content: an id, its owner (so a stranger still gets the 403)
-- and when.
CREATE TABLE IF NOT EXISTS purged_pages (
    id TEXT PRIMARY KEY,
    owner_id TEXT NOT NULL,
    purged_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
