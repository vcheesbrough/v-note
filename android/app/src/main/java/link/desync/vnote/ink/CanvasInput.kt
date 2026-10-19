package link.desync.vnote.ink

import android.view.MotionEvent
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.runtime.withFrameNanos
import androidx.compose.ui.geometry.Offset
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import link.desync.vnote.model.Stroke
import link.desync.vnote.model.StrokePoint
import link.desync.vnote.model.StrokeStyle

// The canvas's touch handling: one MotionEvent stream, split per pointer into
// stylus ink/erasing and finger pan/zoom with momentum.
//
// Both used to be separate Compose input handlers, and the stylus one refused
// any stream that did not *start* with a stylus. Compose's interop filter then
// ignores the rest of that stream, so a pen that landed while a finger or palm
// was already down never drew — its pointer went to the pan handler instead
// (#390). Owning the whole stream here lets each pointer be routed on its own.

private const val NANOS_PER_SECOND = 1_000_000_000f

internal enum class PointerRole { Stylus, Finger, Ignored }

// Which pointer of the current touch stream does what. Pure, so the routing
// rules are unit-tested without MotionEvents.
//
// - The first stylus contact draws, whenever it lands — also mid-pan.
// - Fingers pan until a stylus touches; from then on, until every pointer is
//   up, they are a resting hand and are ignored, so the page cannot slide
//   under the pen.
// - A second stylus contact while one is drawing is ignored.
internal class CanvasPointerRouter {
    var stylusPointerId: Int? = null
        private set
    private val fingerIds = linkedSetOf<Int>()
    private var stylusTouchedThisStream = false

    fun down(
        pointerId: Int,
        isStylus: Boolean,
    ): PointerRole =
        when {
            isStylus && stylusPointerId == null -> {
                stylusPointerId = pointerId
                stylusTouchedThisStream = true
                fingerIds.clear()
                PointerRole.Stylus
            }
            isStylus || stylusTouchedThisStream -> PointerRole.Ignored
            else -> {
                fingerIds += pointerId
                PointerRole.Finger
            }
        }

    fun roleOf(pointerId: Int): PointerRole =
        when (pointerId) {
            stylusPointerId -> PointerRole.Stylus
            in fingerIds -> PointerRole.Finger
            else -> PointerRole.Ignored
        }

    fun up(pointerId: Int): PointerRole {
        val role = roleOf(pointerId)
        if (role == PointerRole.Stylus) {
            stylusPointerId = null
        }
        fingerIds.remove(pointerId)
        return role
    }

    fun reset() {
        stylusPointerId = null
        fingerIds.clear()
        stylusTouchedThisStream = false
    }
}

// What the canvas's current composition says. Written after every composition,
// so the long-lived input handler reads current values without being rebuilt.
internal class CanvasInputs {
    var inkBlocked: Boolean = false
    var selectedTool: CanvasTool = CanvasTool.Drawing
    var drawingStyle: StrokeStyle = StrokeStyle()
    var strokes: List<Stroke> = emptyList()
    var eraserRadiusPx: Float = 0f
    var onStrokeFinished: (Stroke) -> Unit = {}
    var onStrokesErased: (Collection<String>) -> Unit = {}
}

internal class CanvasInputController(
    scope: CoroutineScope,
) {
    val inputs = CanvasInputs()

    // View transform on the infinite canvas. Stored ink never mutates when the
    // viewport pans/zooms — only this transform changes (see PLAN → Canvas & navigation).
    var viewport by mutableStateOf(ViewportTransform())
        private set

    var hoverEraserPoint by mutableStateOf<StrokePoint?>(null)
        private set
    var hoverShowsEraser by mutableStateOf(false)
        private set

    val stylus = StylusGesture(inputs) { viewport }
    private val pan = PanGesture(scope, { viewport }) { viewport = it }
    private val router = CanvasPointerRouter()
    private var hoverButtonEraserArmed = false

    fun onMotionEvent(event: MotionEvent): Boolean =
        when (val action = normalizedSamsungSpenAction(event.actionMasked)) {
            MotionEvent.ACTION_HOVER_ENTER,
            MotionEvent.ACTION_HOVER_MOVE,
            MotionEvent.ACTION_HOVER_EXIT,
            MotionEvent.ACTION_BUTTON_PRESS,
            MotionEvent.ACTION_BUTTON_RELEASE,
            -> onHover(event, action)
            MotionEvent.ACTION_DOWN, MotionEvent.ACTION_POINTER_DOWN -> onPointerDown(event, action)
            MotionEvent.ACTION_MOVE -> onMove(event)
            MotionEvent.ACTION_POINTER_UP, MotionEvent.ACTION_UP -> onPointerUp(event, action)
            MotionEvent.ACTION_CANCEL -> onCancel(event, action)
            else -> stylus.isActive
        }

    fun dispose() {
        pan.stop()
    }

    private fun onHover(
        event: MotionEvent,
        action: Int,
    ): Boolean {
        val toolType = event.getToolType(event.actionIndex.coerceAtLeast(0))
        if (
            !shouldHandleCanvasMotion(
                toolType = toolType,
                action = action,
                activeStylusGesture = stylus.isActive,
                hoverButtonEraserArmed = hoverButtonEraserArmed,
                stylusButtonPressed = event.hasStylusButtonPressed(),
            )
        ) {
            return false
        }
        if (action == MotionEvent.ACTION_HOVER_EXIT) {
            hoverShowsEraser = false
            hoverEraserPoint = null
            return true
        }
        val selectedTool = inputs.selectedTool
        hoverButtonEraserArmed =
            nextHoverButtonEraserArmed(
                currentlyArmed = hoverButtonEraserArmed,
                selectedTool = selectedTool,
                action = action,
                toolType = toolType,
                buttonState = event.buttonState,
                actionButton = event.actionButton,
            )
        hoverShowsEraser =
            if (action == MotionEvent.ACTION_HOVER_ENTER || action == MotionEvent.ACTION_HOVER_MOVE) {
                effectiveCanvasTool(selectedTool, toolType, event.buttonState, hoverButtonEraserArmed) ==
                    CanvasTool.Eraser
            } else {
                selectedTool == CanvasTool.Eraser || hoverButtonEraserArmed
            }
        hoverEraserPoint = if (hoverShowsEraser) event.toWorldPoint(viewport, event.eventTime) else null
        return true
    }

    // Every DOWN is accepted, finger or stylus. Refusing one makes Compose's
    // interop filter skip the rest of the stream, which is how a pen landing
    // beside a resting finger used to be lost.
    private fun onPointerDown(
        event: MotionEvent,
        action: Int,
    ): Boolean {
        if (action == MotionEvent.ACTION_DOWN) {
            router.reset()
            stylus.clear()
        }
        val index = event.actionIndex
        val isStylus =
            shouldHandleCanvasMotion(
                toolType = event.getToolType(index),
                action = MotionEvent.ACTION_DOWN,
                activeStylusGesture = false,
                hoverButtonEraserArmed = hoverButtonEraserArmed,
                stylusButtonPressed = event.hasStylusButtonPressed(),
            )
        when (router.down(event.getPointerId(index), isStylus)) {
            PointerRole.Stylus -> {
                // The pen wins: no pan or fling keeps moving the page under it.
                pan.stop()
                hoverShowsEraser = false
                hoverEraserPoint = null
                // Blocked by another editor: the pen still owns its pointer,
                // so the hand does not pan, but it draws nothing.
                if (!inputs.inkBlocked) {
                    stylus.start(event, index, hoverButtonEraserArmed)
                }
            }
            PointerRole.Finger -> {
                if (action == MotionEvent.ACTION_DOWN) {
                    pan.stop()
                }
                pan.update(event, router)
            }
            PointerRole.Ignored -> Unit
        }
        return true
    }

    private fun onMove(event: MotionEvent): Boolean {
        val stylusId = router.stylusPointerId
        if (stylusId == null) {
            pan.update(event, router)
            return true
        }
        val index = event.findPointerIndex(stylusId)
        if (index >= 0) {
            stylus.move(event, index)
        }
        return true
    }

    private fun onPointerUp(
        event: MotionEvent,
        action: Int,
    ): Boolean {
        val index = event.actionIndex
        when (router.up(event.getPointerId(index))) {
            PointerRole.Stylus -> {
                stylus.finish(event, index)
                hoverButtonEraserArmed = disarmEraserOnLift(event, MotionEvent.ACTION_UP, inputs.selectedTool)
            }
            PointerRole.Finger -> if (action == MotionEvent.ACTION_UP) pan.end() else pan.update(event, router)
            PointerRole.Ignored -> Unit
        }
        if (action == MotionEvent.ACTION_UP) {
            router.reset()
            pan.abandon()
        }
        return true
    }

    private fun onCancel(
        event: MotionEvent,
        action: Int,
    ): Boolean {
        stylus.clear()
        router.reset()
        pan.abandon()
        hoverButtonEraserArmed = disarmEraserOnLift(event, action, inputs.selectedTool)
        return true
    }
}

// One stylus contact: the live stroke under the nib, or the eraser's sweep.
internal class StylusGesture(
    private val inputs: CanvasInputs,
    private val viewport: () -> ViewportTransform,
) {
    val liveStroke = mutableStateListOf<StrokePoint>()
    val liveEraserPath = mutableStateListOf<StrokePoint>()

    // The style captured at touch-down, so a palette change mid-stroke cannot
    // restyle it. Null while erasing or idle.
    var liveStyle by mutableStateOf<StrokeStyle?>(null)
        private set

    private var tool: CanvasTool? = null
    private var startTime = 0L
    private val erasedThisGesture = mutableSetOf<String>()

    val isActive: Boolean get() = tool != null

    fun start(
        event: MotionEvent,
        index: Int,
        hoverButtonEraserArmed: Boolean,
    ) {
        clear()
        startTime = event.eventTime
        val active =
            effectiveCanvasTool(
                inputs.selectedTool,
                event.getToolType(index),
                event.buttonState,
                hoverButtonEraserArmed = hoverButtonEraserArmed,
            )
        tool = active
        liveStyle = inputs.drawingStyle.takeIf { active == CanvasTool.Drawing }
        val point = sample(event, index)
        if (active == CanvasTool.Drawing) liveStroke.add(point) else erase(point)
    }

    fun move(
        event: MotionEvent,
        index: Int,
    ) {
        when (tool) {
            CanvasTool.Drawing -> {
                val capturesPressure = liveStyle != null
                // Replay coalesced samples in order, then the current one —
                // preserving fast pressure changes.
                for (h in 0 until event.historySize) {
                    liveStroke.add(
                        event.historicalWorldPoint(
                            h,
                            viewport(),
                            startTime,
                            event.capturedHistoricalPressure(h, capturesPressure, index),
                            index,
                        ),
                    )
                }
                liveStroke.add(sample(event, index))
            }
            CanvasTool.Eraser -> erase(sample(event, index))
            null -> Unit
        }
    }

    fun finish(
        event: MotionEvent,
        index: Int,
    ) {
        val point = sample(event, index)
        val style = liveStyle
        when (tool) {
            CanvasTool.Drawing -> {
                liveStroke.add(point)
                if (style != null) {
                    inputs.onStrokeFinished(Stroke(points = liveStroke.toList(), style = style))
                }
            }
            CanvasTool.Eraser -> erase(point)
            null -> Unit
        }
        clear()
    }

    fun clear() {
        liveStroke.clear()
        liveEraserPath.clear()
        tool = null
        liveStyle = null
        erasedThisGesture.clear()
    }

    private fun sample(
        event: MotionEvent,
        index: Int,
    ): StrokePoint = event.toWorldPoint(viewport(), startTime, event.capturedPressure(liveStyle != null, index), index)

    private fun erase(point: StrokePoint) {
        eraseAtPoint(
            point,
            liveEraserPath,
            inputs.strokes,
            erasedThisGesture,
            inputs.eraserRadiusPx / viewport().scale,
            inputs.onStrokesErased,
        )
    }
}

// Finger pan and pinch zoom, with momentum after a single-finger fling.
private class PanGesture(
    private val scope: CoroutineScope,
    private val viewport: () -> ViewportTransform,
    private val setViewport: (ViewportTransform) -> Unit,
) {
    private var tracker: ViewportGestureTracker? = null
    private var momentumJob: Job? = null

    // Feed the fingers [router] currently lets pan; starts tracking if idle.
    fun update(
        event: MotionEvent,
        router: CanvasPointerRouter,
    ) {
        val fingers =
            (0 until event.pointerCount).filter { router.roleOf(event.getPointerId(it)) == PointerRole.Finger }
        if (fingers.isEmpty()) {
            return
        }
        val positions = fingers.map { Offset(event.getX(it), event.getY(it)) }
        val centroid = positions.fold(Offset.Zero) { acc, position -> acc + position } / positions.size.toFloat()
        val spread =
            if (positions.size >= 2) positions.sumOf { (it - centroid).getDistance().toDouble() }.toFloat() / positions.size else 0f
        val active = tracker ?: ViewportGestureTracker().also { tracker = it }
        val step = active.update(positions.size, centroid, spread, event.eventTime)
        if (step.panDelta != Offset.Zero || step.zoom != 1f) {
            setViewport(viewport().applyGesture(step.panDelta, step.zoom, step.centroid))
        }
    }

    // The last finger lifted: fling if it was moving fast enough.
    fun end() {
        val ended = tracker ?: return
        tracker = null
        if (ended.shouldLaunchMomentum()) {
            val initial = ended.velocity()
            momentumJob =
                scope.launch {
                    runPanMomentum(initial) { delta -> setViewport(viewport().pan(delta)) }
                }
        }
    }

    // Drop the gesture without a fling.
    fun abandon() {
        tracker = null
    }

    // Stop everything, including a fling still running.
    fun stop() {
        tracker = null
        momentumJob?.cancel()
        momentumJob = null
    }
}

private suspend fun runPanMomentum(
    initialVelocity: Offset,
    onPan: (Offset) -> Unit,
) {
    var velocity = initialVelocity
    var previousFrame = withFrameNanos { it }
    while (kotlin.coroutines.coroutineContext.isActive && shouldContinueMomentum(velocity)) {
        val frame = withFrameNanos { it }
        val deltaSeconds = (frame - previousFrame) / NANOS_PER_SECOND
        previousFrame = frame
        onPan(velocity * deltaSeconds)
        velocity = dampVelocity(velocity, deltaSeconds)
    }
}
