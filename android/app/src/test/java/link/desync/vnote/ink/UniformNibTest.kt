package link.desync.vnote.ink

import link.desync.vnote.model.StrokePoint
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * `buildInkGeometry` picks a round-capped polyline over a filled ribbon on this
 * predicate alone, and the choice is load-bearing: ink migrated up from the
 * retired `solid_round` v1 style carries no pressure anywhere, and a ribbon
 * would give it butt ends and un-rounded joins instead of the round caps v1
 * drew. The predicate is split out of `buildInkGeometry` precisely so it can be
 * asserted here, on the JVM, without constructing an `android.graphics.Path`.
 */
class UniformNibTest {
    private fun point(
        x: Double,
        pressure: Double?,
    ) = StrokePoint(x = x, y = 0.0, t = 0, pressure = pressure)

    /** Migrated v1 ink: no pressure on any point. */
    @Test
    fun pressurelessStrokeUsesTheUniformNib() {
        val migrated = listOf(point(0.0, null), point(10.0, null), point(20.0, null))
        assertTrue(usesUniformNib(migrated))
    }

    /** A stylus-authored stroke: pressure throughout. */
    @Test
    fun pressuredStrokeDoesNotUseTheUniformNib() {
        val authored = listOf(point(0.0, 0.2), point(10.0, 0.6), point(20.0, 1.0))
        assertFalse(usesUniformNib(authored))
    }

    /**
     * A single pressured point among bare ones is enough to take the ribbon —
     * the nib is no longer uniform, so the polyline's constant width would be
     * wrong for part of the stroke.
     */
    @Test
    fun onePressuredPointIsEnoughToTakeTheRibbon() {
        assertFalse(usesUniformNib(listOf(point(0.0, null), point(10.0, 0.5), point(20.0, null))))
        assertFalse(usesUniformNib(listOf(point(0.0, 0.5), point(10.0, null))))
        assertFalse(usesUniformNib(listOf(point(0.0, null), point(10.0, 0.0))))
    }

    /**
     * Vacuously true for an empty list. Not reachable through
     * `buildInkGeometry`, which returns null on `points.isEmpty()` first, but
     * pinned here rather than left to inspection.
     */
    @Test
    fun emptyStrokeIsVacuouslyUniform() {
        assertTrue(usesUniformNib(emptyList()))
    }
}
