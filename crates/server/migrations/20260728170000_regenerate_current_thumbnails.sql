-- Iteration 21 (#281): the thumbnail renderer no longer collapses letter-sized
-- strokes into circles. Thumbnails are cached per ink revision and only
-- re-render when the ink changes, so flip every page's current-revision
-- thumbnail back to 'generating'; recover_pending re-enqueues them at startup
-- and they re-render with the fixed renderer. Historical revisions are left
-- alone — nothing serves them as the page preview.
UPDATE page_thumbnails AS t
SET status = 'generating', png = NULL
FROM pages AS p
WHERE p.id = t.page_id
  AND t.source_seq = p.ink_revision
  AND t.status <> 'generating';
