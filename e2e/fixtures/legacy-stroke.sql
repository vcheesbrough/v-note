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

INSERT INTO pages (id, owner_id, title, created_at, updated_at, ink_revision)
VALUES (
    'page_revision_history',
    'v-note-test-service-account',
    'Revision migration fixture',
    '2026-07-01T12:00:00Z',
    '2026-07-01T12:00:00Z',
    3
)
ON CONFLICT (id) DO NOTHING;

INSERT INTO stroke_batches (page_id, seq, client_batch_id, strokes)
VALUES
    (
        'page_revision_history',
        1,
        'history-batch-1',
        '[{"id":"history-stroke-1","style":{"tool_kind":"solid_round","style_version":1,"parameters":{"color":"#006400","width":4.0,"cap_style":"round","join_style":"round"}},"points":[{"x":40.0,"y":40.0,"t":0},{"x":160.0,"y":120.0,"t":12}]}]'::jsonb
    ),
    (
        'page_revision_history',
        2,
        'history-batch-2',
        '[{"id":"history-stroke-2","style":{"tool_kind":"solid_round","style_version":1,"parameters":{"color":"#006400","width":4.0,"cap_style":"round","join_style":"round"}},"points":[{"x":180.0,"y":140.0,"t":0},{"x":320.0,"y":220.0,"t":12}]}]'::jsonb
    )
ON CONFLICT (page_id, seq) DO NOTHING;

INSERT INTO stroke_tombstones (page_id, stroke_id, deleted_revision)
VALUES ('page_revision_history', 'history-stroke-1', 2)
ON CONFLICT (page_id, stroke_id) DO NOTHING;

INSERT INTO tombstone_batches (page_id, client_mutation_id, revision, stroke_ids)
VALUES ('page_revision_history', 'history-erase-1', 2, '["history-stroke-1"]'::jsonb)
ON CONFLICT (page_id, client_mutation_id) DO NOTHING;
