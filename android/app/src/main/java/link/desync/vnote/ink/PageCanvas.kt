package link.desync.vnote.ink

import android.view.MotionEvent
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.outlined.ArrowBack
import androidx.compose.material.icons.outlined.DeleteSweep
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.FilledTonalIconToggleButton
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Slider
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.runtime.withFrameNanos
import androidx.compose.ui.Alignment
import androidx.compose.ui.ExperimentalComposeUiApi
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clipToBounds
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Path
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.StrokeJoin
import androidx.compose.ui.graphics.drawscope.DrawScope
import androidx.compose.ui.graphics.drawscope.withTransform
import androidx.compose.ui.input.pointer.AwaitPointerEventScope
import androidx.compose.ui.input.pointer.PointerInputChange
import androidx.compose.ui.input.pointer.PointerType
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.input.pointer.pointerInteropFilter
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.selected
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import link.desync.vnote.auth.ApiClient
import link.desync.vnote.auth.PageSummary
import link.desync.vnote.auth.Stroke
import link.desync.vnote.auth.StrokePoint
import link.desync.vnote.auth.StrokeStyle
import link.desync.vnote.ui.displayTitle
import kotlinx.coroutines.Job
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import java.util.Locale
import kotlin.math.abs
import kotlin.math.min
import kotlin.math.roundToInt
import kotlin.math.sqrt
import androidx.compose.ui.graphics.drawscope.Stroke as DrawStroke

private const val NANOS_PER_SECOND = 1_000_000_000f
private const val MIN_WIDTH = 1f
private const val MAX_WIDTH = 32f
private const val WIDTH_INCREMENT = 0.5f
private val ToolColors =
    listOf(
        "#000000",
        "#4B5563",
        "#006400",
        "#00796B",
        "#1565C0",
        "#6A1B9A",
        "#C62828",
        "#EF6C00",
    )

internal enum class CanvasTool { Drawing, Eraser }

// Full-screen ink editor for one open page: top bar with back + title, an
// optional status banner (e.g. blocked-by-another-editor), and the infinite
// canvas. Connects the page channel on enter and releases it on exit.
@Composable
fun PageCanvasScreen(
    apiClient: ApiClient,
    page: PageSummary,
    userId: String,
    onBack: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val scope = rememberCoroutineScope()
    val context = LocalContext.current
    val toolPreferences = remember(userId) { DrawingToolPreferences(context, userId) }
    val session = remember(page.id) { PageInkSession(apiClient, page.id, scope) }
    var selectedTool by remember(page.id) { mutableStateOf(CanvasTool.Drawing) }
    var paletteOpen by remember(page.id) { mutableStateOf(false) }
    var drawingStyle by remember(userId) { mutableStateOf(toolPreferences.load()) }
    DisposableEffect(page.id) {
        session.connect()
        onDispose { session.disconnect() }
    }

    Column(modifier = modifier.fillMaxSize()) {
        Row(
            modifier = Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 8.dp),
            horizontalArrangement = Arrangement.spacedBy(12.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            IconButton(onClick = onBack) {
                Icon(Icons.AutoMirrored.Outlined.ArrowBack, contentDescription = "Back to pages")
            }
            Text(
                page.displayTitle(),
                modifier = Modifier.weight(1f),
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                style = MaterialTheme.typography.titleMedium,
            )
            DrawingToolControl(
                style = drawingStyle,
                selected = selectedTool == CanvasTool.Drawing,
                paletteOpen = paletteOpen,
                onClick = {
                    if (selectedTool == CanvasTool.Eraser) {
                        selectedTool = CanvasTool.Drawing
                        paletteOpen = false
                    } else {
                        paletteOpen = !paletteOpen
                    }
                },
                onPaletteDismiss = { paletteOpen = false },
                onStyleChange = { style ->
                    drawingStyle = style
                    toolPreferences.save(style)
                },
            )
            FilledTonalIconToggleButton(
                checked = selectedTool == CanvasTool.Eraser,
                onCheckedChange = {
                    selectedTool = CanvasTool.Eraser
                    paletteOpen = false
                },
                modifier =
                    Modifier
                        .size(48.dp)
                        .testTag("eraser-tool")
                        .semantics { selected = selectedTool == CanvasTool.Eraser },
            ) {
                Icon(Icons.Outlined.DeleteSweep, contentDescription = "Eraser")
            }
        }
        session.statusBanner?.let { banner ->
            Text(
                banner,
                modifier = Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 4.dp),
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.error,
            )
        }
        InkCanvas(
            strokes = session.strokes,
            canEdit = session.canEdit,
            selectedTool = selectedTool,
            drawingStyle = drawingStyle,
            onStrokeFinished = session::commitStroke,
            onStrokesErased = session::eraseStrokes,
            modifier = Modifier.fillMaxWidth().weight(1f),
        )
    }
}

@Composable
private fun DrawingToolControl(
    style: StrokeStyle,
    selected: Boolean,
    paletteOpen: Boolean,
    onClick: () -> Unit,
    onPaletteDismiss: () -> Unit,
    onStyleChange: (StrokeStyle) -> Unit,
) {
    Box {
        FilledTonalIconToggleButton(
            checked = selected,
            onCheckedChange = { onClick() },
            modifier =
                Modifier
                    .size(48.dp)
                    .testTag("drawing-tool")
                    .semantics {
                        this.selected = selected
                        contentDescription = "Drawing tool"
                    },
        ) {
            ToolStrokePreview(style = style, modifier = Modifier.size(width = 32.dp, height = 24.dp))
        }
        DropdownMenu(
            expanded = paletteOpen,
            onDismissRequest = onPaletteDismiss,
            modifier = Modifier.width(224.dp).testTag("tool-palette"),
        ) {
            Text(
                "Colour",
                modifier = Modifier.padding(horizontal = 12.dp, vertical = 4.dp),
                style = MaterialTheme.typography.labelLarge,
            )
            ToolColors.chunked(4).forEach { rowColors ->
                Row(modifier = Modifier.padding(horizontal = 4.dp)) {
                    rowColors.forEach { color ->
                        ColorSwatch(
                            color = color,
                            selected = style.parameters.color == color,
                            onClick = {
                                onStyleChange(
                                    style.copy(parameters = style.parameters.copy(color = color)),
                                )
                            },
                        )
                    }
                }
            }
            Row(
                modifier = Modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 4.dp),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                ToolStrokePreview(
                    style = style,
                    modifier = Modifier.size(width = 72.dp, height = 32.dp),
                )
                Text(
                    String.format(Locale.US, "%.1f", style.parameters.width),
                    modifier = Modifier.testTag("tool-width-value"),
                    style = MaterialTheme.typography.labelLarge,
                )
            }
            Slider(
                value = style.parameters.width.toFloat(),
                onValueChange = { raw ->
                    val stepped =
                        (raw / WIDTH_INCREMENT).roundToInt() * WIDTH_INCREMENT
                    onStyleChange(
                        style.copy(parameters = style.parameters.copy(width = stepped.toDouble())),
                    )
                },
                valueRange = MIN_WIDTH..MAX_WIDTH,
                steps = 61,
                modifier =
                    Modifier
                        .fillMaxWidth()
                        .padding(horizontal = 12.dp)
                        .testTag("tool-width-slider")
                        .semantics { contentDescription = "Stroke width" },
            )
        }
    }
}

@Composable
private fun ColorSwatch(
    color: String,
    selected: Boolean,
    onClick: () -> Unit,
) {
    Box(
        modifier =
            Modifier
                .size(48.dp)
                .testTag("swatch-$color")
                .semantics {
                    this.selected = selected
                    contentDescription = "Colour $color"
                }
                .clickable(onClick = onClick),
        contentAlignment = Alignment.Center,
    ) {
        Box(
            modifier =
                Modifier
                    .size(32.dp)
                    .background(parseColor(color), CircleShape)
                    .then(
                        if (selected) {
                            Modifier.border(3.dp, MaterialTheme.colorScheme.onSurface, CircleShape)
                        } else {
                            Modifier.border(1.dp, MaterialTheme.colorScheme.outline, CircleShape)
                        },
                    ),
        )
    }
}

@Composable
private fun ToolStrokePreview(
    style: StrokeStyle,
    modifier: Modifier,
) {
    Surface(modifier = modifier) {
        Canvas(modifier = Modifier.fillMaxSize().padding(2.dp)) {
            val previewWidth = style.parameters.width.toFloat().coerceIn(2f, size.height - 2f)
            drawLine(
                color = parseColor(style.parameters.color),
                start = Offset(2f, size.height / 2f),
                end = Offset(size.width - 2f, size.height / 2f),
                strokeWidth = previewWidth,
                cap = StrokeCap.Round,
            )
        }
    }
}

@Composable
@OptIn(ExperimentalComposeUiApi::class)
private fun InkCanvas(
    strokes: List<Stroke>,
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
    DisposableEffect(Unit) {
        onDispose { momentumJob?.cancel() }
    }

    Canvas(
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
                            val sensitive = capturedDrawingStyle?.isPressureSensitive == true
                            val point =
                                event.toWorldPoint(
                                    viewport,
                                    stylusStartTime,
                                    event.capturedPressure(sensitive),
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
                                val sensitive = capturedDrawingStyle?.isPressureSensitive == true
                                // Replay coalesced samples in order, then the
                                // current one — preserving fast pressure changes.
                                for (h in 0 until event.historySize) {
                                    liveStroke.add(
                                        event.historicalWorldPoint(
                                            h,
                                            viewport,
                                            stylusStartTime,
                                            event.capturedHistoricalPressure(h, sensitive),
                                        ),
                                    )
                                }
                                liveStroke.add(
                                    event.toWorldPoint(
                                        viewport,
                                        stylusStartTime,
                                        event.capturedPressure(sensitive),
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
                            val sensitive = capturedDrawingStyle?.isPressureSensitive == true
                            val point =
                                event.toWorldPoint(
                                    viewport,
                                    stylusStartTime,
                                    event.capturedPressure(sensitive),
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
                }
                .pointerInput(canEdit) {
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
        withTransform({
            translate(viewport.offset.x, viewport.offset.y)
            scale(viewport.scale, viewport.scale, pivot = Offset.Zero)
        }) {
            for (stroke in strokes) {
                drawInk(
                    stroke.points,
                    stroke.style,
                    parseColor(stroke.style.parameters.color),
                )
            }
            if (liveStroke.isNotEmpty()) {
                val liveStyle = capturedDrawingStyle ?: drawingStyle
                drawInk(
                    liveStroke,
                    liveStyle,
                    parseColor(liveStyle.parameters.color),
                )
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
                (action == MotionEvent.ACTION_MOVE ||
                    action == MotionEvent.ACTION_UP ||
                    action == MotionEvent.ACTION_CANCEL)
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
): CanvasTool {
    return if (
        selectedTool == CanvasTool.Eraser ||
        toolType == MotionEvent.TOOL_TYPE_ERASER ||
        buttonState and STYLUS_ERASER_BUTTON_MASK != 0 ||
        hoverButtonEraserArmed
    ) {
        CanvasTool.Eraser
    } else {
        CanvasTool.Drawing
    }
}

private const val STYLUS_ERASER_BUTTON_MASK =
    MotionEvent.BUTTON_STYLUS_PRIMARY or
        MotionEvent.BUTTON_STYLUS_SECONDARY or
        MotionEvent.BUTTON_SECONDARY or
        MotionEvent.BUTTON_TERTIARY

private fun MotionEvent.hasStylusButtonPressed(): Boolean =
    buttonState and STYLUS_ERASER_BUTTON_MASK != 0

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

/// Recompute the armed eraser latch at stylus lift so it cannot stay stuck when
/// the misclassified Samsung contact stream never emits an ACTION_BUTTON_RELEASE.
private fun disarmEraserOnLift(
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

private fun MotionEvent.toWorldPoint(
    viewport: ViewportTransform,
    startTime: Long,
    pressure: Double? = null,
): StrokePoint {
    val world = viewport.toWorld(Offset(x, y))
    return StrokePoint(world.x.toDouble(), world.y.toDouble(), eventTime - startTime, pressure)
}

// World-space sample for a historical (coalesced) pointer position in this
// event. Between frames Android batches several samples; capturing them keeps
// fast pressure changes and fine geometry from being lost.
private fun MotionEvent.historicalWorldPoint(
    index: Int,
    viewport: ViewportTransform,
    startTime: Long,
    pressure: Double?,
): StrokePoint {
    val world = viewport.toWorld(Offset(getHistoricalX(index), getHistoricalY(index)))
    return StrokePoint(
        world.x.toDouble(),
        world.y.toDouble(),
        getHistoricalEventTime(index) - startTime,
        pressure,
    )
}

// Normalise a raw stylus pressure sample for the wire: null when the active
// style ignores pressure (v1), otherwise clamped to 0.0..1.0 (raw pressure can
// exceed 1.0 on some devices, and NaN must never reach the contract).
internal fun normalizePressure(
    raw: Float,
    sensitive: Boolean,
): Double? {
    if (!sensitive) return null
    if (raw.isNaN()) return 0.0
    return raw.toDouble().coerceIn(0.0, 1.0)
}

private fun MotionEvent.capturedPressure(sensitive: Boolean): Double? =
    normalizePressure(pressure, sensitive)

private fun MotionEvent.capturedHistoricalPressure(index: Int, sensitive: Boolean): Double? =
    normalizePressure(getHistoricalPressure(index), sensitive)

private fun eraseAtPoint(
    point: StrokePoint,
    livePath: MutableList<StrokePoint>,
    strokes: List<Stroke>,
    erasedThisGesture: MutableSet<String>,
    eraserRadius: Float,
    onStrokesErased: (Collection<String>) -> Unit,
) {
    val sweep = livePath.lastOrNull()?.let { previous -> listOf(previous, point) } ?: listOf(point)
    livePath.add(point)
    val newHits = findIntersectedStrokes(strokes, sweep, eraserRadius) - erasedThisGesture
    if (newHits.isNotEmpty()) {
        erasedThisGesture.addAll(newHits)
        onStrokesErased(newHits)
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

private fun DrawScope.drawInk(
    points: List<StrokePoint>,
    style: StrokeStyle,
    color: Color,
) {
    if (points.isEmpty()) {
        return
    }
    if (points.size == 1) {
        drawCircle(
            color = color,
            radius = (style.renderedWidth(points[0].pressure) / 2.0).toFloat(),
            center = Offset(points[0].x.toFloat(), points[0].y.toFloat()),
        )
        return
    }
    // Constant-width (v1) ink: a single round-capped polyline. Byte-identical to
    // the pre-pressure renderer.
    if (!style.isPressureSensitive) {
        val path = Path()
        path.moveTo(points[0].x.toFloat(), points[0].y.toFloat())
        for (index in 1 until points.size) {
            path.lineTo(points[index].x.toFloat(), points[index].y.toFloat())
        }
        drawPath(
            path = path,
            color = color,
            style =
                DrawStroke(
                    width = style.parameters.width.toFloat(),
                    cap = StrokeCap.Round,
                    join = StrokeJoin.Round,
                ),
        )
        return
    }
    // Pressure-modulated (v2) ink: build one filled variable-width ribbon and
    // draw it in a single call. Per-segment stroking was O(points) draw calls
    // per stroke, re-run for every committed stroke every frame — the source of
    // the multi-stroke latency. A single fill restores ~constant-width cost.
    drawPath(path = buildPressureRibbon(points, style), color = color)
}

// One filled polygon approximating a variable-width stroke: walk the left offset
// forward, then the right offset back, and close (flat end caps). Per-vertex
// averaged normals keep joins smooth. Filled once (NonZero), this replaces the
// O(points) per-segment stroking that scaled badly across many strokes.
private fun buildPressureRibbon(points: List<StrokePoint>, style: StrokeStyle): Path {
    val n = points.size
    val px = FloatArray(n) { points[it].x.toFloat() }
    val py = FloatArray(n) { points[it].y.toFloat() }
    val radius = FloatArray(n) { (style.renderedWidth(points[it].pressure) / 2.0).toFloat() }

    // Left-side unit normal per vertex, averaged from the incident segments so
    // the offset edges meet smoothly at joins.
    val nx = FloatArray(n)
    val ny = FloatArray(n)
    for (i in 0 until n) {
        var ax = 0f
        var ay = 0f
        if (i > 0) {
            val dx = px[i] - px[i - 1]
            val dy = py[i] - py[i - 1]
            val len = sqrt(dx * dx + dy * dy)
            if (len > 1e-3f) {
                ax += -dy / len
                ay += dx / len
            }
        }
        if (i < n - 1) {
            val dx = px[i + 1] - px[i]
            val dy = py[i + 1] - py[i]
            val len = sqrt(dx * dx + dy * dy)
            if (len > 1e-3f) {
                ax += -dy / len
                ay += dx / len
            }
        }
        val len = sqrt(ax * ax + ay * ay)
        if (len > 1e-3f) {
            nx[i] = ax / len
            ny[i] = ay / len
        } else {
            nx[i] = 0f
            ny[i] = 1f
        }
    }

    val path = Path()
    path.moveTo(px[0] + nx[0] * radius[0], py[0] + ny[0] * radius[0])
    for (i in 1 until n) {
        path.lineTo(px[i] + nx[i] * radius[i], py[i] + ny[i] * radius[i])
    }
    for (i in n - 1 downTo 0) {
        path.lineTo(px[i] - nx[i] * radius[i], py[i] - ny[i] * radius[i])
    }
    path.close()
    return path
}

private fun parseColor(value: String): Color = Color(android.graphics.Color.parseColor(value))

internal fun findIntersectedStrokes(
    strokes: List<Stroke>,
    eraserPath: List<StrokePoint>,
    eraserRadius: Float,
): Set<String> {
    if (eraserPath.isEmpty()) return emptySet()
    return strokes
        .asSequence()
        .filter { stroke ->
            val hitRadius = eraserRadius.toDouble() + stroke.style.parameters.width / 2.0
            pathsWithinDistance(eraserPath, stroke.points, hitRadius)
        }
        .mapTo(linkedSetOf()) { it.id }
}

private fun pathsWithinDistance(
    first: List<StrokePoint>,
    second: List<StrokePoint>,
    distance: Double,
): Boolean {
    if (first.isEmpty() || second.isEmpty()) return false
    val firstSegments = pathSegments(first)
    val secondSegments = pathSegments(second)
    return firstSegments.any { a ->
        secondSegments.any { b -> segmentDistance(a.first, a.second, b.first, b.second) <= distance }
    }
}

private fun pathSegments(points: List<StrokePoint>): List<Pair<StrokePoint, StrokePoint>> =
    if (points.size == 1) listOf(points[0] to points[0]) else points.zipWithNext()

private fun segmentDistance(
    a: StrokePoint,
    b: StrokePoint,
    c: StrokePoint,
    d: StrokePoint,
): Double {
    if (segmentsIntersect(a, b, c, d)) return 0.0
    return min(
        min(pointSegmentDistance(a, c, d), pointSegmentDistance(b, c, d)),
        min(pointSegmentDistance(c, a, b), pointSegmentDistance(d, a, b)),
    )
}

private fun segmentsIntersect(
    a: StrokePoint,
    b: StrokePoint,
    c: StrokePoint,
    d: StrokePoint,
): Boolean {
    val abC = cross(a, b, c)
    val abD = cross(a, b, d)
    val cdA = cross(c, d, a)
    val cdB = cross(c, d, b)
    if (abs(abC) < 1e-9 && onSegment(a, b, c)) return true
    if (abs(abD) < 1e-9 && onSegment(a, b, d)) return true
    if (abs(cdA) < 1e-9 && onSegment(c, d, a)) return true
    if (abs(cdB) < 1e-9 && onSegment(c, d, b)) return true
    return (abC > 0) != (abD > 0) && (cdA > 0) != (cdB > 0)
}

private fun cross(
    a: StrokePoint,
    b: StrokePoint,
    point: StrokePoint,
): Double = (b.x - a.x) * (point.y - a.y) - (b.y - a.y) * (point.x - a.x)

private fun onSegment(
    a: StrokePoint,
    b: StrokePoint,
    point: StrokePoint,
): Boolean =
    point.x in minOf(a.x, b.x)..maxOf(a.x, b.x) &&
        point.y in minOf(a.y, b.y)..maxOf(a.y, b.y)

private fun pointSegmentDistance(
    point: StrokePoint,
    start: StrokePoint,
    end: StrokePoint,
): Double {
    val dx = end.x - start.x
    val dy = end.y - start.y
    if (dx == 0.0 && dy == 0.0) {
        return sqrt((point.x - start.x) * (point.x - start.x) + (point.y - start.y) * (point.y - start.y))
    }
    val projection =
        (((point.x - start.x) * dx + (point.y - start.y) * dy) / (dx * dx + dy * dy))
            .coerceIn(0.0, 1.0)
    val closestX = start.x + projection * dx
    val closestY = start.y + projection * dy
    return sqrt((point.x - closestX) * (point.x - closestX) + (point.y - closestY) * (point.y - closestY))
}
