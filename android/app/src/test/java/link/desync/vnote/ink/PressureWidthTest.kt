package link.desync.vnote.ink

import link.desync.vnote.model.MIN_PRESSURE_WIDTH
import link.desync.vnote.model.SOLID_ROUND_PRESSURE_STYLE_VERSION
import link.desync.vnote.model.SolidRoundParameters
import link.desync.vnote.model.StrokeStyle
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The Android pressure→width curve and pressure clamping must stay identical to
 * `protocol::StrokeStyle::rendered_width` and the server's validation posture.
 */
class PressureWidthTest {
    private val v2 =
        StrokeStyle(
            styleVersion = SOLID_ROUND_PRESSURE_STYLE_VERSION,
            parameters = SolidRoundParameters(width = 4.0),
        )
    private val v1 = StrokeStyle(parameters = SolidRoundParameters(width = 4.0))

    @Test
    fun v2RenderedWidthFollowsSharedCurve() {
        assertTrue(v2.isPressureSensitive)
        assertEquals(4.0, v2.renderedWidth(1.0), 1e-9)
        assertEquals(4.0, v2.renderedWidth(null), 1e-9) // absent pressure = full width
        assertEquals(MIN_PRESSURE_WIDTH, v2.renderedWidth(0.0), 1e-9) // absolute floor
        val mid = MIN_PRESSURE_WIDTH + (4.0 - MIN_PRESSURE_WIDTH) * 0.5
        assertEquals(mid, v2.renderedWidth(0.5), 1e-9)
        // Out-of-range pressure is clamped for rendering.
        assertEquals(4.0, v2.renderedWidth(5.0), 1e-9)
        assertEquals(MIN_PRESSURE_WIDTH, v2.renderedWidth(-1.0), 1e-9)

        // A wide pen still tapers to the same thin floor at light pressure.
        val wide = v2.copy(parameters = v2.parameters.copy(width = 32.0))
        assertEquals(MIN_PRESSURE_WIDTH, wide.renderedWidth(0.0), 1e-9)
        assertEquals(32.0, wide.renderedWidth(1.0), 1e-9)
    }

    @Test
    fun v1IgnoresPressure() {
        assertFalse(v1.isPressureSensitive)
        assertEquals(4.0, v1.renderedWidth(0.0), 1e-9)
        assertEquals(4.0, v1.renderedWidth(1.0), 1e-9)
        assertEquals(4.0, v1.renderedWidth(null), 1e-9)
    }

    @Test
    fun normalizePressureClampsAndGatesOnStyle() {
        // Not pressure-sensitive → always null (v1 points omit pressure).
        assertNull(normalizePressure(0.5f, sensitive = false))
        assertNull(normalizePressure(1f, sensitive = false))
        // Sensitive → clamped into 0.0..1.0.
        assertEquals(0.0, normalizePressure(-0.2f, sensitive = true)!!, 1e-6)
        assertEquals(1.0, normalizePressure(2.5f, sensitive = true)!!, 1e-6)
        assertEquals(0.5, normalizePressure(0.5f, sensitive = true)!!, 1e-6)
        // NaN collapses to zero, never reaching the wire as non-finite.
        assertEquals(0.0, normalizePressure(Float.NaN, sensitive = true)!!, 1e-6)
    }
}
