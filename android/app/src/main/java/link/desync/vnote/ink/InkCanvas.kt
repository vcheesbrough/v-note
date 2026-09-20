package link.desync.vnote.ink

import android.view.MotionEvent
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.runtime.withFrameNanos
import androidx.compose.ui.ExperimentalComposeUiApi
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clipToBounds
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.drawscope.withTransform
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.input.pointer.AwaitPointerEventScope
import androidx.compose.ui.input.pointer.PointerInputChange
import androidx.compose.ui.input.pointer.PointerType
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.input.pointer.pointerInteropFilter
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.unit.dp
import androidx.tracing.trace
import kotlinx.coroutines.Job
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import link.desync.vnote.model.Stroke
import link.desync.vnote.model.StrokePoint
import link.desync.vnote.model.StrokeStyle
import androidx.compose.ui.graphics.drawscope.Stroke as DrawStroke

// The infinite ink canvas: stylus capture and erasing, finger pan/zoom with
// momentum, and the committed and live render layers.

private const val NANOS_PER_SECOND = 1_000_000_000f

@Composable
@OptIn(ExperimentalComposeUiApi::class)
internal fun InkCanvas(
    strokes: List<Stroke>,
    paper: Paper,
    canEdit: Boolean,
    selectedTool: CanvasTool,
    drawingStyle: StrokeStyle,
    onStrokeFinished: (Stroke) -> Unit,
    onStrokesErased: (Collection<String>) -> Unit,
    modifier: Modifier = Modifier,
) {
    // View transform on the infinite canvas. Stored ink never mutates when the
    // viewport pans/zooms — only this transform changes (see PLAN → Canvas & navigation).
    var viewport by remember { mutableStateOf(ViewportTransform()) }
    val scope = rememberCoroutineScope()
    var momentumJob by remember { mutableStateOf<Job?>(null) }
    val liveStroke = remember { mutableStateListOf<StrokePoint>() }
    val liveEraserPath = remember { mutableStateListOf<StrokePoint>() }
    var activeStylusTool by remember { mutableStateOf<CanvasTool?>(null) }
    var capturedDrawingStyle by remember { mutableStateOf<StrokeStyle?>(null) }
    var stylusStartTime by remember { mutableStateOf(0L) }
    val erasedThisGesture = remember { mutableSetOf<String>() }
    var hoverEraserPoint by remember { mutableStateOf<StrokePoint?>(null) }
    var hoverShowsEraser by remember { mutableStateOf(false) }
    var hoverButtonEraserArmed by remember { mutableStateOf(false) }
    val eraserRadiusPx = with(LocalDensity.current) { 12.dp.toPx() }
    // Committed ink is rebuilt only when a stroke arrives, leaves or changes
    // style. Deliberately not Compose state: it is touched during the draw
    // phase, where a write must not schedule a recomposition.
    val strokeGeometry = remember { StrokeGeometryCache() }
    DisposableEffect(Unit) {
        onDispose { momentumJob?.cancel() }
    }

    Box(
        modifier =
            modifier
                .clipToBounds()
                .background(Color.White)
                .testTag("ink-canvas")
                .pointerInteropFilter { event ->
                    val action = normalizedSamsungSpenAction(event.actionMasked)
                    if (
                        !shouldHandleCanvasMotion(
                            toolType = event.getToolType(event.actionIndex.coerceAtLeast(0)),
                            action = action,
                            activeStylusGesture = activeStylusTool != null,
                            hoverButtonEraserArmed = hoverButtonEraserArmed,
                            stylusButtonPressed = event.hasStylusButtonPressed(),
                        )
                    ) {
                        return@pointerInteropFilter false
                    }
                    when (action) {
                        MotionEvent.ACTION_HOVER_ENTER,
                        MotionEvent.ACTION_HOVER_MOVE,
                        -> {
                            hoverButtonEraserArmed =
                                nextHoverButtonEraserArmed(
                                    currentlyArmed = hoverButtonEraserArmed,
                                    selectedTool = selectedTool,
                                    action = action,
                                    toolType = event.getToolType(event.actionIndex),
                                    buttonState = event.buttonState,
                                    actionButton = event.actionButton,
                                )
                            val hoverTool =
                                effectiveCanvasTool(
                                    selectedTool,
                                    event.getToolType(event.actionIndex),
                                    event.buttonState,
                                    hoverButtonEraserArmed = hoverButtonEraserArmed,
                                )
                            hoverShowsEraser = hoverTool == CanvasTool.Eraser
                            hoverEraserPoint =
                                if (hoverShowsEraser) {
                                    event.toWorldPoint(viewport, event.eventTime)
                                } else {
                                    null
                                }
                            true
                        }
                        MotionEvent.ACTION_BUTTON_PRESS,
                        MotionEvent.ACTION_BUTTON_RELEASE,
                        -> {
                            hoverButtonEraserArmed =
                                nextHoverButtonEraserArmed(
                                    currentlyArmed = hoverButtonEraserArmed,
                                    selectedTool = selectedTool,
                                    action = action,
                                    toolType = event.getToolType(event.actionIndex),
                                    buttonState = event.buttonState,
                                    actionButton = event.actionButton,
                                )
                            hoverShowsEraser = selectedTool == CanvasTool.Eraser || hoverButtonEraserArmed
                            hoverEraserPoint =
                                if (hoverShowsEraser) {
                                    event.toWorldPoint(viewport, event.eventTime)
                                } else {
                                    null
                                }
                            true
                        }
                        MotionEvent.ACTION_HOVER_EXIT -> {
                            hoverShowsEraser = false
                            hoverEraserPoint = null
                            true
                        }
                        MotionEvent.ACTION_DOWN -> {
                            if (!canEdit) {
                                return@pointerInteropFilter false
                            }
                            momentumJob?.cancel()
                            momentumJob = null
                            stylusStartTime = event.eventTime
                            activeStylusTool =
                                effectiveCanvasTool(
                                    selectedTool,
                                    event.getToolType(event.actionIndex),
                                    event.buttonState,
                                    hoverButtonEraserArmed = hoverButtonEraserArmed,
                                )
                            capturedDrawingStyle =
                                drawingStyle.takeIf { activeStylusTool == CanvasTool.Drawing }
                            liveStroke.clear()
                            liveEraserPath.clear()
                            erasedThisGesture.clear()
                            val capturesPressure = capturedDrawingStyle != null
                            val point =
                                event.toWorldPoint(
                                    viewport,
                                    stylusStartTime,
                                    event.capturedPressure(capturesPressure),
                                )
                            if (activeStylusTool == CanvasTool.Drawing) {
                                liveStroke.add(point)
                            } else {
                                hoverShowsEraser = false
                                hoverEraserPoint = null
                                eraseAtPoint(
                                    point,
                                    liveEraserPath,
                                    strokes,
                                    erasedThisGesture,
                                    eraserRadiusPx / viewport.scale,
                                    onStrokesErased,
                                )
                            }
                            true
                        }
                        MotionEvent.ACTION_MOVE -> {
                            if (activeStylusTool == null) {
                                return@pointerInteropFilter false
                            }
                            if (activeStylusTool == CanvasTool.Drawing) {
                                val capturesPressure = capturedDrawingStyle != null
                                // Replay coalesced samples in order, then the
                                // current one — preserving fast pressure changes.
                                for (h in 0 until event.historySize) {
                                    liveStroke.add(
                                        event.historicalWorldPoint(
                                            h,
                                            viewport,
                                            stylusStartTime,
                                            event.capturedHistoricalPressure(h, capturesPressure),
                                        ),
                                    )
                                }
                                liveStroke.add(
                                    event.toWorldPoint(
                                        viewport,
                                        stylusStartTime,
                                        event.capturedPressure(capturesPressure),
                                    ),
                                )
                            } else {
                                eraseAtPoint(
                                    event.toWorldPoint(viewport, stylusStartTime),
                                    liveEraserPath,
                                    strokes,
                                    erasedThisGesture,
                                    eraserRadiusPx / viewport.scale,
                                    onStrokesErased,
                                )
                            }
                            true
                        }
                        MotionEvent.ACTION_UP -> {
                            val tool = activeStylusTool ?: return@pointerInteropFilter false
                            val capturesPressure = capturedDrawingStyle != null
                            val point =
                                event.toWorldPoint(
                                    viewport,
                                    stylusStartTime,
                                    event.capturedPressure(capturesPressure),
                                )
                            if (tool == CanvasTool.Drawing) {
                                liveStroke.add(point)
                                val captured = liveStroke.toList()
                                val style = capturedDrawingStyle
                                if (captured.isNotEmpty() && style != null) {
                                    onStrokeFinished(Stroke(points = captured, style = style))
                                }
                            } else {
                                eraseAtPoint(
                                    point,
                                    liveEraserPath,
                                    strokes,
                                    erasedThisGesture,
                                    eraserRadiusPx / viewport.scale,
                                    onStrokesErased,
                                )
                            }
                            liveStroke.clear()
                            liveEraserPath.clear()
                            activeStylusTool = null
                            capturedDrawingStyle = null
                            erasedThisGesture.clear()
                            hoverButtonEraserArmed = disarmEraserOnLift(event, action, selectedTool)
                            true
                        }
                        MotionEvent.ACTION_CANCEL -> {
                            liveStroke.clear()
                            liveEraserPath.clear()
                            activeStylusTool = null
                            capturedDrawingStyle = null
                            erasedThisGesture.clear()
                            hoverButtonEraserArmed = disarmEraserOnLift(event, action, selectedTool)
                            true
                        }
                        else -> activeStylusTool != null
                    }
                }.pointerInput(canEdit) {
                    awaitEachGesture {
                        val firstDown = awaitFirstDown(requireUnconsumed = false)
                        if (
                            activeStylusTool != null ||
                            firstDown.type == PointerType.Stylus ||
                            firstDown.type == PointerType.Eraser
                        ) {
                            return@awaitEachGesture
                        }
                        momentumJob?.cancel()
                        momentumJob = null
                        val gesture =
                            handleViewport { panDelta, zoom, centroid ->
                                viewport = viewport.applyGesture(panDelta, zoom, centroid)
                            }
                        if (gesture.launchMomentum) {
                            momentumJob =
                                scope.launch {
                                    runPanMomentum(gesture.velocity) { delta ->
                                        viewport = viewport.pan(delta)
                                    }
                                }
                        }
                    }
                },
    ) {
        // Paper and committed ink, in their own render layer.
        //
        // Splitting them off the live stroke is what keeps inking cheap: a draw
        // node is invalidated by the state *it* reads, so a stylus sample now
        // dirties only the live canvas below. This layer's display list is
        // replayed as-is instead of re-recorded, and its draw block — the one
        // whose cost grows with the page — does not run again until the ink,
        // the paper or the viewport actually changes.
        Canvas(modifier = Modifier.fillMaxSize().graphicsLayer()) {
            trace("v-note:ink-committed") {
                val canvasSize = size
                // Grain first, and deliberately *outside* the transform: it tiles in
                // device space so it keeps a constant size at every zoom, unlike the
                // rules, which are world-anchored and ride the ink.
                drawPaperTexture(paper)
                withTransform({
                    translate(viewport.offset.x, viewport.offset.y)
                    scale(viewport.scale, viewport.scale, pivot = Offset.Zero)
                }) {
                    // Paper first, under the ink transform, so it stays locked to the
                    // ink through pan and zoom and can never overpaint a stroke.
                    drawPaperMarks(paper, viewport, canvasSize)
                    strokeGeometry.visitRenderable(strokes) { geometry, color ->
                        drawInkGeometry(geometry, color)
                    }
                }
            }
        }
        // The stroke under the nib, plus the eraser cursor — everything that
        // changes between frames of a single gesture.
        Canvas(modifier = Modifier.fillMaxSize()) {
            trace("v-note:ink-live") {
                withTransform({
                    translate(viewport.offset.x, viewport.offset.y)
                    scale(viewport.scale, viewport.scale, pivot = Offset.Zero)
                }) {
                    if (liveStroke.isNotEmpty()) {
                        val liveStyle = capturedDrawingStyle ?: drawingStyle
                        drawInk(liveStroke, liveStyle, parseColor(liveStyle.parameters.color))
                    }
                    val eraserCursor = liveEraserPath.lastOrNull() ?: hoverEraserPoint.takeIf { hoverShowsEraser }
                    if (eraserCursor != null) {
                        drawCircle(
                            color = Color.Black,
                            radius = eraserRadiusPx / viewport.scale,
                            center = Offset(eraserCursor.x.toFloat(), eraserCursor.y.toFloat()),
                            style = DrawStroke(width = 1.dp.toPx() / viewport.scale),
                        )
                    }
                }
            }
        }
    }
}

// Single-finger pan + two-finger pinch zoom; reports incremental transforms.
private suspend fun AwaitPointerEventScope.handleViewport(
    onTransform: (panDelta: Offset, zoom: Float, centroid: Offset) -> Unit,
): ViewportGestureResult {
    val tracker = ViewportGestureTracker()
    while (true) {
        val event = awaitPointerEvent()
        val pressed = event.changes.filter { it.pressed }
        if (pressed.isEmpty()) {
            return ViewportGestureResult(
                velocity = tracker.velocity(),
                launchMomentum = tracker.shouldLaunchMomentum(),
            )
        }
        val centroid =
            pressed.fold(Offset.Zero) { acc, change -> acc + change.position } /
                pressed.size.toFloat()
        val spread = if (pressed.size >= 2) averageSpread(pressed, centroid) else 0f
        val step =
            tracker.update(
                pointerCount = pressed.size,
                centroid = centroid,
                spread = spread,
                eventTimeMillis = event.changes.maxOf { it.uptimeMillis },
            )
        if (step.panDelta != Offset.Zero || step.zoom != 1f) {
            onTransform(step.panDelta, step.zoom, step.centroid)
        }
        pressed.forEach { it.consume() }
    }
}

private data class ViewportGestureResult(
    val velocity: Offset,
    val launchMomentum: Boolean,
)

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

private fun averageSpread(
    changes: List<PointerInputChange>,
    centroid: Offset,
): Float {
    if (changes.isEmpty()) {
        return 0f
    }
    var total = 0f
    for (change in changes) {
        total += (change.position - centroid).getDistance()
    }
    return total / changes.size
}

private fun toWorldPoint(
    position: Offset,
    viewport: ViewportTransform,
    startTime: Long,
): StrokePoint {
    val world = viewport.toWorld(position)
    return StrokePoint(
        x = world.x.toDouble(),
        y = world.y.toDouble(),
        t = System.currentTimeMillis() - startTime,
    )
}
