package link.desync.vnote.ink

import android.view.MotionEvent
import link.desync.vnote.auth.SolidRoundParameters
import link.desync.vnote.auth.Stroke
import link.desync.vnote.auth.StrokePoint
import link.desync.vnote.auth.StrokeStyle
import org.junit.Assert.assertEquals
import org.junit.Test

class PageCanvasTest {
    @Test
    fun effectiveToolUsesExplicitButtonAndEraserEndInputs() {
        assertEquals(
            CanvasTool.Drawing,
            effectiveCanvasTool(CanvasTool.Drawing, MotionEvent.TOOL_TYPE_STYLUS, 0),
        )
        assertEquals(
            CanvasTool.Eraser,
            effectiveCanvasTool(CanvasTool.Eraser, MotionEvent.TOOL_TYPE_STYLUS, 0),
        )
        assertEquals(
            CanvasTool.Eraser,
            effectiveCanvasTool(
                CanvasTool.Drawing,
                MotionEvent.TOOL_TYPE_STYLUS,
                MotionEvent.BUTTON_STYLUS_PRIMARY,
            ),
        )
        assertEquals(
            CanvasTool.Eraser,
            effectiveCanvasTool(
                CanvasTool.Drawing,
                MotionEvent.TOOL_TYPE_STYLUS,
                MotionEvent.BUTTON_STYLUS_SECONDARY,
            ),
        )
        assertEquals(
            CanvasTool.Eraser,
            effectiveCanvasTool(
                CanvasTool.Drawing,
                MotionEvent.TOOL_TYPE_STYLUS,
                MotionEvent.BUTTON_SECONDARY,
            ),
        )
        assertEquals(
            CanvasTool.Eraser,
            effectiveCanvasTool(CanvasTool.Drawing, MotionEvent.TOOL_TYPE_ERASER, 0),
        )
        assertEquals(
            CanvasTool.Eraser,
            effectiveCanvasTool(
                CanvasTool.Drawing,
                MotionEvent.TOOL_TYPE_STYLUS,
                0,
                hoverButtonEraserArmed = true,
            ),
        )
    }

    @Test
    fun hoverButtonLatchCoversTipDownWhenHardwareDropsButtonState() {
        val latchUntil =
            hoverButtonEraserLatchUntil(
                CanvasTool.Eraser,
                MotionEvent.BUTTON_STYLUS_PRIMARY,
                eventTime = 1_000L,
            )

        assertEquals(1_500L, latchUntil)
        assertEquals(true, isHoverButtonEraserLatchActive(1_499L, latchUntil))
        assertEquals(
            CanvasTool.Eraser,
            effectiveCanvasTool(
                CanvasTool.Drawing,
                MotionEvent.TOOL_TYPE_STYLUS,
                buttonState = 0,
                hoverButtonEraserArmed = isHoverButtonEraserLatchActive(1_499L, latchUntil),
            ),
        )
        assertEquals(false, isHoverButtonEraserLatchActive(1_501L, latchUntil))
        assertEquals(
            0L,
            hoverButtonEraserLatchUntil(CanvasTool.Drawing, MotionEvent.BUTTON_STYLUS_PRIMARY, 1_000L),
        )
        assertEquals(0L, hoverButtonEraserLatchUntil(CanvasTool.Eraser, buttonState = 0, 1_000L))
    }

    @Test
    fun sweptEraserFindsFastCrossingAndEveryOverlappingStroke() {
        val horizontal = stroke("horizontal", 0.0, 50.0, 100.0, 50.0)
        val vertical = stroke("vertical", 50.0, 0.0, 50.0, 100.0)
        val distant = stroke("distant", 0.0, 150.0, 100.0, 150.0)

        val hits =
            findIntersectedStrokes(
                listOf(horizontal, vertical, distant),
                listOf(point(0.0, 0.0), point(100.0, 100.0)),
                eraserRadius = 12f,
            )

        assertEquals(setOf("horizontal", "vertical"), hits)
    }

    @Test
    fun strokeWidthContributesToWholeStrokeHitArea() {
        val wide =
            Stroke(
                id = "wide",
                style = StrokeStyle(parameters = SolidRoundParameters(width = 32.0)),
                points = listOf(point(0.0, 30.0), point(100.0, 30.0)),
            )

        assertEquals(
            setOf("wide"),
            findIntersectedStrokes(
                listOf(wide),
                listOf(point(0.0, 0.0), point(100.0, 0.0)),
                eraserRadius = 14f,
            ),
        )
    }

    @Test
    fun singlePointEraserGestureHitsSinglePointStroke() {
        val dot = Stroke(id = "dot", points = listOf(point(10.0, 10.0)))

        assertEquals(
            setOf("dot"),
            findIntersectedStrokes(listOf(dot), listOf(point(20.0, 10.0)), eraserRadius = 8f),
        )
    }

    private fun stroke(
        id: String,
        x1: Double,
        y1: Double,
        x2: Double,
        y2: Double,
    ): Stroke = Stroke(id = id, points = listOf(point(x1, y1), point(x2, y2)))

    private fun point(
        x: Double,
        y: Double,
    ): StrokePoint = StrokePoint(x, y, 0)
}
