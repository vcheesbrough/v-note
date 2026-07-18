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
    fun normalizesSamsungButtonHeldContactActions() {
        assertEquals(MotionEvent.ACTION_DOWN, normalizedSamsungSpenAction(211))
        assertEquals(MotionEvent.ACTION_UP, normalizedSamsungSpenAction(212))
        assertEquals(MotionEvent.ACTION_MOVE, normalizedSamsungSpenAction(213))
        assertEquals(MotionEvent.ACTION_CANCEL, normalizedSamsungSpenAction(214))
        assertEquals(MotionEvent.ACTION_HOVER_MOVE, normalizedSamsungSpenAction(MotionEvent.ACTION_HOVER_MOVE))
    }

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
    fun hoverButtonStaysArmedUntilHardwareReportsRelease() {
        val armed =
            nextHoverButtonEraserArmed(
                currentlyArmed = false,
                selectedTool = CanvasTool.Drawing,
                action = MotionEvent.ACTION_HOVER_MOVE,
                toolType = MotionEvent.TOOL_TYPE_STYLUS,
                buttonState = MotionEvent.BUTTON_STYLUS_PRIMARY,
            )

        assertEquals(true, armed)
        assertEquals(
            true,
            nextHoverButtonEraserArmed(
                currentlyArmed = false,
                selectedTool = CanvasTool.Drawing,
                action = MotionEvent.ACTION_BUTTON_PRESS,
                toolType = MotionEvent.TOOL_TYPE_STYLUS,
                buttonState = 0,
                actionButton = MotionEvent.BUTTON_STYLUS_PRIMARY,
            ),
        )
        assertEquals(
            CanvasTool.Eraser,
            effectiveCanvasTool(
                CanvasTool.Drawing,
                MotionEvent.TOOL_TYPE_STYLUS,
                buttonState = 0,
                hoverButtonEraserArmed = armed,
            ),
        )
        assertEquals(
            true,
            nextHoverButtonEraserArmed(
                currentlyArmed = armed,
                selectedTool = CanvasTool.Drawing,
                action = MotionEvent.ACTION_DOWN,
                toolType = MotionEvent.TOOL_TYPE_FINGER,
                buttonState = 0,
            ),
        )
        assertEquals(
            false,
            nextHoverButtonEraserArmed(
                currentlyArmed = armed,
                selectedTool = CanvasTool.Drawing,
                action = MotionEvent.ACTION_BUTTON_RELEASE,
                toolType = MotionEvent.TOOL_TYPE_STYLUS,
                buttonState = 0,
                actionButton = MotionEvent.BUTTON_STYLUS_PRIMARY,
            ),
        )
        assertEquals(
            false,
            nextHoverButtonEraserArmed(
                currentlyArmed = true,
                selectedTool = CanvasTool.Eraser,
                action = MotionEvent.ACTION_HOVER_MOVE,
                toolType = MotionEvent.TOOL_TYPE_ERASER,
                buttonState = MotionEvent.BUTTON_STYLUS_PRIMARY,
            ),
        )
    }

    @Test
    fun armedHoverOwnsMisclassifiedContactUntilLift() {
        assertEquals(
            true,
            shouldHandleCanvasMotion(
                toolType = MotionEvent.TOOL_TYPE_FINGER,
                action = MotionEvent.ACTION_DOWN,
                activeStylusGesture = false,
                hoverButtonEraserArmed = true,
            ),
        )
        assertEquals(
            true,
            shouldHandleCanvasMotion(
                toolType = MotionEvent.TOOL_TYPE_FINGER,
                action = MotionEvent.ACTION_MOVE,
                activeStylusGesture = true,
                hoverButtonEraserArmed = false,
            ),
        )
        assertEquals(
            true,
            shouldHandleCanvasMotion(
                toolType = MotionEvent.TOOL_TYPE_UNKNOWN,
                action = MotionEvent.ACTION_UP,
                activeStylusGesture = true,
                hoverButtonEraserArmed = false,
            ),
        )
        assertEquals(
            true,
            shouldHandleCanvasMotion(
                toolType = MotionEvent.TOOL_TYPE_FINGER,
                action = MotionEvent.ACTION_DOWN,
                activeStylusGesture = false,
                hoverButtonEraserArmed = false,
                stylusButtonPressed = true,
            ),
        )
        assertEquals(
            false,
            shouldHandleCanvasMotion(
                toolType = MotionEvent.TOOL_TYPE_FINGER,
                action = MotionEvent.ACTION_DOWN,
                activeStylusGesture = false,
                hoverButtonEraserArmed = false,
            ),
        )
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
