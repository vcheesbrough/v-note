package link.desync.vnote.ink

import android.view.MotionEvent
import androidx.compose.runtime.getValue
import androidx.compose.runtime.setValue

// Stylus event classification: which motions the canvas owns, Samsung S Pen action
// normalisation, and the eraser tool/button latch. Pure over event fields.

internal fun shouldHandleCanvasMotion(
    toolType: Int,
    action: Int,
    activeStylusGesture: Boolean,
    hoverButtonEraserArmed: Boolean,
    stylusButtonPressed: Boolean = false,
): Boolean {
    val isStylus =
        toolType == MotionEvent.TOOL_TYPE_STYLUS || toolType == MotionEvent.TOOL_TYPE_ERASER
    return when {
        isStylus -> true
        action == MotionEvent.ACTION_DOWN -> hoverButtonEraserArmed || stylusButtonPressed
        else ->
            activeStylusGesture &&
                (
                    action == MotionEvent.ACTION_MOVE ||
                        action == MotionEvent.ACTION_UP ||
                        action == MotionEvent.ACTION_CANCEL
                )
    }
}

internal fun normalizedSamsungSpenAction(action: Int): Int =
    when (action) {
        SAMSUNG_SPEN_ACTION_DOWN -> MotionEvent.ACTION_DOWN
        SAMSUNG_SPEN_ACTION_UP -> MotionEvent.ACTION_UP
        SAMSUNG_SPEN_ACTION_MOVE -> MotionEvent.ACTION_MOVE
        SAMSUNG_SPEN_ACTION_CANCEL -> MotionEvent.ACTION_CANCEL
        else -> action
    }

private const val SAMSUNG_SPEN_ACTION_DOWN = 211
private const val SAMSUNG_SPEN_ACTION_UP = 212
private const val SAMSUNG_SPEN_ACTION_MOVE = 213
private const val SAMSUNG_SPEN_ACTION_CANCEL = 214

internal fun effectiveCanvasTool(
    selectedTool: CanvasTool,
    toolType: Int,
    buttonState: Int,
    hoverButtonEraserArmed: Boolean = false,
): CanvasTool =
    if (
        selectedTool == CanvasTool.Eraser ||
        toolType == MotionEvent.TOOL_TYPE_ERASER ||
        buttonState and STYLUS_ERASER_BUTTON_MASK != 0 ||
        hoverButtonEraserArmed
    ) {
        CanvasTool.Eraser
    } else {
        CanvasTool.Drawing
    }

private const val STYLUS_ERASER_BUTTON_MASK =
    MotionEvent.BUTTON_STYLUS_PRIMARY or
        MotionEvent.BUTTON_STYLUS_SECONDARY or
        MotionEvent.BUTTON_SECONDARY or
        MotionEvent.BUTTON_TERTIARY

internal fun MotionEvent.hasStylusButtonPressed(): Boolean = buttonState and STYLUS_ERASER_BUTTON_MASK != 0

internal fun nextHoverButtonEraserArmed(
    currentlyArmed: Boolean,
    selectedTool: CanvasTool,
    action: Int,
    toolType: Int,
    buttonState: Int,
    actionButton: Int = 0,
): Boolean {
    if (selectedTool == CanvasTool.Eraser) return false
    return when (action) {
        MotionEvent.ACTION_BUTTON_RELEASE ->
            if (actionButton and STYLUS_ERASER_BUTTON_MASK != 0) false else currentlyArmed
        MotionEvent.ACTION_BUTTON_PRESS,
        MotionEvent.ACTION_HOVER_ENTER,
        MotionEvent.ACTION_HOVER_MOVE,
        ->
            toolType == MotionEvent.TOOL_TYPE_ERASER ||
                buttonState and STYLUS_ERASER_BUTTON_MASK != 0 ||
                actionButton and STYLUS_ERASER_BUTTON_MASK != 0
        // A lift ends the gesture. The Samsung path reports the contact stream as
        // mouse/finger and may never emit an ACTION_BUTTON_RELEASE, so re-derive
        // from the hardware button at lift: stay armed only while the button is
        // still physically held, otherwise disarm so the next plain contact draws
        // instead of erasing. A fresh hover/press re-arms it.
        MotionEvent.ACTION_UP,
        MotionEvent.ACTION_CANCEL,
        ->
            buttonState and STYLUS_ERASER_BUTTON_MASK != 0 ||
                actionButton and STYLUS_ERASER_BUTTON_MASK != 0
        else -> currentlyArmed
    }
}

// / Recompute the armed eraser latch at stylus lift so it cannot stay stuck when
// / the misclassified Samsung contact stream never emits an ACTION_BUTTON_RELEASE.
internal fun disarmEraserOnLift(
    event: MotionEvent,
    action: Int,
    selectedTool: CanvasTool,
): Boolean =
    nextHoverButtonEraserArmed(
        currentlyArmed = false,
        selectedTool = selectedTool,
        action = action,
        toolType = event.getToolType(event.actionIndex.coerceAtLeast(0)),
        buttonState = event.buttonState,
        actionButton = event.actionButton,
    )
