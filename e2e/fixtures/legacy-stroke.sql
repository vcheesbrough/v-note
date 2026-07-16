INSERT INTO pages (id, owner_id, title, created_at, updated_at, ink_revision)
VALUES (
    'page_legacy_v2',
    'v-note-test-service-account',
    'Legacy protocol 2 page',
    '2026-07-01T12:00:00Z',
    '2026-07-01T12:00:00Z',
    1
)
ON CONFLICT (id) DO NOTHING;

INSERT INTO stroke_batches (page_id, seq, client_batch_id, strokes)
VALUES (
    'page_legacy_v2',
    1,
    'legacy-batch-1',
    '[{"tool":"pen","color":"#006400","width":4.0,"points":[{"x":40.0,"y":40.0,"t":0},{"x":360.0,"y":220.0,"t":12}]}]'::jsonb
)
ON CONFLICT (page_id, seq) DO UPDATE
SET strokes = EXCLUDED.strokes;
