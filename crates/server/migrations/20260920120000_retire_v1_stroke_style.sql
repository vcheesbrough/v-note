-- Iteration 48 (#389) retires the `solid_round` v1 style: a constant-width nib
-- on which per-point `pressure` was forbidden. v2 — pressure-modulated — is now
-- the only style the protocol accepts, so any stored v1 stroke would fail
-- validation and vanish from its page on the next replay.
--
-- Lift them in place. Only the discriminator changes: points keep whatever they
-- had, which for v1 ink is no `pressure` at all. That is lossless because a v2
-- point without pressure renders at full `width`
-- (`StrokeStyle::rendered_width(None) == parameters.width`) — exactly what v1's
-- constant-width nib drew. No page is deleted and no geometry is rewritten.
--
-- `20260716003000_backfill_legacy_strokes.sql` is what minted most of these
-- rows, emitting `style_version: 1` for pre-style ink. It stays untouched —
-- it is applied history; this migration corrects its output rather than
-- rewriting the past.
--
-- Idempotent: the WHERE clause stops matching once every stroke is v2, and
-- re-running the UPDATE over already-v2 rows is a no-op.
UPDATE stroke_batches AS batch
SET strokes = (
    SELECT jsonb_agg(
        CASE
            WHEN stroke #> '{style,style_version}' = '1'::jsonb
                THEN jsonb_set(stroke, '{style,style_version}', '2'::jsonb)
            ELSE stroke
        END
        ORDER BY ordinal
    )
    FROM jsonb_array_elements(batch.strokes) WITH ORDINALITY AS entry(stroke, ordinal)
)
WHERE EXISTS (
    SELECT 1
    FROM jsonb_array_elements(batch.strokes) AS candidate(stroke)
    WHERE stroke #> '{style,style_version}' = '1'::jsonb
);
