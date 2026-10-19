package link.desync.vnote.ink

import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.SideEffect
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.ui.ExperimentalComposeUiApi
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clipToBounds
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.drawscope.withTransform
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.input.pointer.pointerInteropFilter
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.unit.dp
import androidx.tracing.trace
import link.desync.vnote.model.Stroke
import link.desync.vnote.model.StrokeStyle
import androidx.compose.ui.graphics.drawscope.Stroke as DrawStroke

// The infinite ink canvas: the committed and live render layers over the
// touch handling in CanvasInput.kt (stylus capture and erasing, finger
// pan/zoom with momentum).
//
// Ink is captured whether or not the edit lease is held yet (#390): the
// session decides what happens to a finished stroke, so the canvas never
// drops a pen-down because a lease grant is still a round trip away. Only
// [inkBlocked] stops it: the session knows it cannot edit (another editor
// holds the lease, or the channel failed).

@Composable
@OptIn(ExperimentalComposeUiApi::class)
internal fun InkCanvas(
    strokes: List<Stroke>,
    paper: Paper,
    inkBlocked: Boolean,
    selectedTool: CanvasTool,
    drawingStyle: StrokeStyle,
    onStrokeFinished: (Stroke) -> Unit,
    onStrokesErased: (Collection<String>) -> Unit,
    modifier: Modifier = Modifier,
) {
    val scope = rememberCoroutineScope()
    // One handler for the canvas's lifetime: it is never rebuilt, so a gesture
    // in progress survives recomposition, and it reads the current tool, style
    // and ink from [CanvasInputs], refreshed after every composition.
    val input = remember { CanvasInputController(scope) }
    val eraserRadiusPx = with(LocalDensity.current) { 12.dp.toPx() }
    SideEffect {
        input.inputs.inkBlocked = inkBlocked
        input.inputs.selectedTool = selectedTool
        input.inputs.drawingStyle = drawingStyle
        input.inputs.strokes = strokes
        input.inputs.eraserRadiusPx = eraserRadiusPx
        input.inputs.onStrokeFinished = onStrokeFinished
        input.inputs.onStrokesErased = onStrokesErased
    }
    DisposableEffect(Unit) {
        onDispose { input.dispose() }
    }

    Box(
        modifier =
            modifier
                .clipToBounds()
                .background(Color.White)
                .testTag("ink-canvas")
                .pointerInteropFilter { event -> input.onMotionEvent(event) },
    ) {
        CommittedInkLayer(input, paper, strokes)
        LiveInkLayer(input, drawingStyle, eraserRadiusPx)
    }
}

// Paper and committed ink, in their own render layer.
//
// Splitting them off the live stroke is what keeps inking cheap: a draw
// node is invalidated by the state *it* reads, so a stylus sample now
// dirties only the live canvas below. This layer's display list is
// replayed as-is instead of re-recorded, and its draw block — the one
// whose cost grows with the page — does not run again until the ink,
// the paper or the viewport actually changes.
@Composable
private fun CommittedInkLayer(
    input: CanvasInputController,
    paper: Paper,
    strokes: List<Stroke>,
) {
    // Committed ink is rebuilt only when a stroke arrives, leaves or changes
    // style. Deliberately not Compose state: it is touched during the draw
    // phase, where a write must not schedule a recomposition.
    val strokeGeometry = remember { StrokeGeometryCache() }
    Canvas(modifier = Modifier.fillMaxSize().graphicsLayer()) {
        trace("v-note:ink-committed") {
            val viewport = input.viewport
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
}

// The stroke under the nib, plus the eraser cursor — everything that
// changes between frames of a single gesture.
@Composable
private fun LiveInkLayer(
    input: CanvasInputController,
    drawingStyle: StrokeStyle,
    eraserRadiusPx: Float,
) {
    val stylus = input.stylus
    Canvas(modifier = Modifier.fillMaxSize()) {
        trace("v-note:ink-live") {
            val viewport = input.viewport
            withTransform({
                translate(viewport.offset.x, viewport.offset.y)
                scale(viewport.scale, viewport.scale, pivot = Offset.Zero)
            }) {
                if (stylus.liveStroke.isNotEmpty()) {
                    val liveStyle = stylus.liveStyle ?: drawingStyle
                    drawInk(stylus.liveStroke, liveStyle, parseColor(liveStyle.parameters.color))
                }
                val eraserCursor =
                    stylus.liveEraserPath.lastOrNull() ?: input.hoverEraserPoint.takeIf { input.hoverShowsEraser }
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
