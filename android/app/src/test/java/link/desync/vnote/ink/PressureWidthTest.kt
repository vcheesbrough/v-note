package link.desync.vnote.ink

import link.desync.vnote.model.MIN_PRESSURE_WIDTH
import link.desync.vnote.model.SOLID_ROUND_PRESSURE_STYLE_VERSION
import link.desync.vnote.model.SolidRoundParameters
import link.desync.vnote.model.StrokeStyle
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

/**
 * The Android pressure→width curve and pressure clamping must stay identical to
 * `protocol::StrokeStyle::rendered_width` and the server's validation posture.
 */
class PressureWidthTest {
    private val style =
        StrokeStyle(
            styleVersion = SOLID_ROUND_PRESSURE_STYLE_VERSION,
            parameters = SolidRoundParameters(width = 4.0),
        )

    @Test
    fun renderedWidthFollowsSharedCurve() {
        assertEquals(4.0, style.renderedWidth(1.0), 1e-9)
        assertEquals(4.0, style.renderedWidth(null), 1e-9) // absent pressure = full width
        assertEquals(MIN_PRESSURE_WIDTH, style.renderedWidth(0.0), 1e-9) // absolute floor
        val mid = MIN_PRESSURE_WIDTH + (4.0 - MIN_PRESSURE_WIDTH) * 0.5
        assertEquals(mid, style.renderedWidth(0.5), 1e-9)
        // Out-of-range pressure is clamped for rendering.
        assertEquals(4.0, style.renderedWidth(5.0), 1e-9)
        assertEquals(MIN_PRESSURE_WIDTH, style.renderedWidth(-1.0), 1e-9)

        // A wide pen still tapers to the same thin floor at light pressure.
        val wide = style.copy(parameters = style.parameters.copy(width = 32.0))
        assertEquals(MIN_PRESSURE_WIDTH, wide.renderedWidth(0.0), 1e-9)
        assertEquals(32.0, wide.renderedWidth(1.0), 1e-9)
    }

    /**
     * Ink migrated up from the retired v1 style carries no pressure on any
     * point, and must keep the constant full width v1's nib drew.
     */
    @Test
    fun pressurelessInkKeepsConstantFullWidth() {
        val widths = List(3) { style.renderedWidth(null) }
        assertEquals(listOf(4.0, 4.0, 4.0), widths)
    }

    /** The default style is the only version the server accepts. */
    @Test
    fun defaultStyleIsThePressureVersion() {
        assertEquals(SOLID_ROUND_PRESSURE_STYLE_VERSION, StrokeStyle().styleVersion)
    }

    @Test
    fun normalizePressureClampsAndGatesOnCapture() {
        // Not capturing pressure → always null.
        assertNull(normalizePressure(0.5f, capturesPressure = false))
        assertNull(normalizePressure(1f, capturesPressure = false))
        // Capturing → clamped into 0.0..1.0.
        assertEquals(0.0, normalizePressure(-0.2f, capturesPressure = true)!!, 1e-6)
        assertEquals(1.0, normalizePressure(2.5f, capturesPressure = true)!!, 1e-6)
        assertEquals(0.5, normalizePressure(0.5f, capturesPressure = true)!!, 1e-6)
        // NaN collapses to zero, never reaching the wire as non-finite.
        assertEquals(0.0, normalizePressure(Float.NaN, capturesPressure = true)!!, 1e-6)
    }
}
