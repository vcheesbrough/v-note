-- The first protocol-v3 migration introduced the new tables but did not
-- rewrite existing JSONB stroke payloads. Backfill them here in a separate
-- migration so databases that already applied that migration are repaired.
-- The id derives only from immutable page/batch/stroke identity and is stable
-- when this statement is run repeatedly.
UPDATE stroke_batches AS batch
SET strokes = (
    SELECT jsonb_agg(
        CASE
            WHEN stroke ? 'id' AND stroke ? 'style' THEN stroke
            ELSE jsonb_build_object(
                'id', COALESCE(
                    stroke ->> 'id',
                    'stroke_legacy_' || md5(
                        batch.page_id || ':' || batch.client_batch_id || ':' ||
                        batch.seq::text || ':' || ordinal::text
                    )
                ),
                'style', COALESCE(
                    stroke -> 'style',
                    jsonb_build_object(
                        'tool_kind', 'solid_round',
                        'style_version', 1,
                        'parameters', jsonb_build_object(
                            'color', upper(COALESCE(stroke ->> 'color', '#006400')),
                            'width', COALESCE(stroke -> 'width', '4.0'::jsonb),
                            'cap_style', 'round',
                            'join_style', 'round'
                        )
                    )
                ),
                'points', COALESCE(stroke -> 'points', '[]'::jsonb)
            )
        END
        ORDER BY ordinal
    )
    FROM jsonb_array_elements(batch.strokes) WITH ORDINALITY AS legacy(stroke, ordinal)
)
WHERE EXISTS (
    SELECT 1
    FROM jsonb_array_elements(batch.strokes) AS candidate(stroke)
    WHERE NOT (stroke ? 'id' AND stroke ? 'style')
);
