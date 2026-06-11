package link.desync.vnote.ink

import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.runtime.snapshots.SnapshotStateList
import androidx.compose.ui.Alignment
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
import androidx.compose.ui.unit.dp
import link.desync.vnote.auth.ApiClient
import link.desync.vnote.auth.PageSummary
import link.desync.vnote.auth.Stroke
import link.desync.vnote.auth.StrokePoint
import androidx.compose.ui.graphics.drawscope.Stroke as StrokeStyle

private val InkColor = Color(0xFF006400)

// Full-screen ink editor for one open page: top bar with back + title, an
// optional status banner (e.g. blocked-by-another-editor), and the infinite
// canvas. Connects the page channel on enter and releases it on exit.
@Composable
fun PageCanvasScreen(
    apiClient: ApiClient,
    page: PageSummary,
    onBack: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val scope = rememberCoroutineScope()
    val session = remember(page.id) { PageInkSession(apiClient, page.id, scope) }
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
            Button(onClick = onBack) { Text("Back") }
            Text(page.title, style = MaterialTheme.typography.titleMedium)
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
            onStrokeFinished = session::commitStroke,
            modifier = Modifier.fillMaxWidth().weight(1f),
        )
    }
}

@Composable
private fun InkCanvas(
    strokes: List<Stroke>,
    canEdit: Boolean,
    onStrokeFinished: (Stroke) -> Unit,
    modifier: Modifier = Modifier,
) {
    // View transform on the infinite canvas. Stored ink never mutates when the
    // viewport pans/zooms — only this transform changes (see PLAN → Canvas & navigation).
    var scale by remember { mutableStateOf(1f) }
    var offset by remember { mutableStateOf(Offset.Zero) }
    val liveStroke = remember { mutableStateListOf<StrokePoint>() }

    Canvas(
        modifier =
            modifier
                .clipToBounds()
                .background(Color.White)
                .pointerInput(canEdit) {
                    awaitEachGesture {
                        val down = awaitFirstDown(requireUnconsumed = false)
                        if (down.type == PointerType.Stylus && canEdit) {
                            // Stylus draws; touch is reserved for viewport navigation.
                            captureStroke(down, scale, offset, liveStroke) {
                                val captured = liveStroke.toList()
                                liveStroke.clear()
                                if (captured.isNotEmpty()) {
                                    onStrokeFinished(Stroke(points = captured))
                                }
                            }
                        } else {
                            handleViewport { panDelta, zoom, centroid ->
                                offset += panDelta
                                if (zoom != 1f) {
                                    offset = centroid - (centroid - offset) * zoom
                                    scale *= zoom
                                }
                            }
                        }
                    }
                },
    ) {
        withTransform({
            translate(offset.x, offset.y)
            scale(scale, scale, pivot = Offset.Zero)
        }) {
            for (stroke in strokes) {
                drawInk(stroke.points, stroke.width.toFloat())
            }
            if (liveStroke.isNotEmpty()) {
                drawInk(liveStroke, Stroke.PEN_WIDTH.toFloat())
            }
        }
    }
}

// Collect one stylus stroke as world-space samples until the pointer lifts.
private suspend fun AwaitPointerEventScope.captureStroke(
    first: PointerInputChange,
    scale: Float,
    offset: Offset,
    liveStroke: SnapshotStateList<StrokePoint>,
    onFinished: () -> Unit,
) {
    val startTime = System.currentTimeMillis()
    liveStroke.clear()
    liveStroke.add(toWorldPoint(first.position, scale, offset, startTime))
    first.consume()
    while (true) {
        val event = awaitPointerEvent()
        val change = event.changes.firstOrNull { it.id == first.id } ?: break
        if (!change.pressed) {
            change.consume()
            break
        }
        liveStroke.add(toWorldPoint(change.position, scale, offset, startTime))
        change.consume()
    }
    onFinished()
}

// Single-finger pan + two-finger pinch zoom; reports incremental transforms.
private suspend fun AwaitPointerEventScope.handleViewport(onTransform: (panDelta: Offset, zoom: Float, centroid: Offset) -> Unit) {
    var previousCentroid: Offset? = null
    var previousSpread = 0f
    while (true) {
        val event = awaitPointerEvent()
        val pressed = event.changes.filter { it.pressed }
        if (pressed.isEmpty()) {
            break
        }
        val centroid =
            pressed.fold(Offset.Zero) { acc, change -> acc + change.position } /
                pressed.size.toFloat()
        val spread = if (pressed.size >= 2) averageSpread(pressed, centroid) else 0f
        val zoom = if (spread > 0f && previousSpread > 0f) spread / previousSpread else 1f
        val panDelta = previousCentroid?.let { centroid - it } ?: Offset.Zero
        if (panDelta != Offset.Zero || zoom != 1f) {
            onTransform(panDelta, zoom, centroid)
        }
        previousCentroid = centroid
        previousSpread = spread
        pressed.forEach { it.consume() }
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
    scale: Float,
    offset: Offset,
    startTime: Long,
): StrokePoint =
    StrokePoint(
        x = ((position.x - offset.x) / scale).toDouble(),
        y = ((position.y - offset.y) / scale).toDouble(),
        t = System.currentTimeMillis() - startTime,
    )

private fun DrawScope.drawInk(
    points: List<StrokePoint>,
    width: Float,
) {
    if (points.isEmpty()) {
        return
    }
    if (points.size == 1) {
        drawCircle(
            color = InkColor,
            radius = width / 2f,
            center = Offset(points[0].x.toFloat(), points[0].y.toFloat()),
        )
        return
    }
    val path = Path()
    path.moveTo(points[0].x.toFloat(), points[0].y.toFloat())
    for (index in 1 until points.size) {
        path.lineTo(points[index].x.toFloat(), points[index].y.toFloat())
    }
    drawPath(
        path = path,
        color = InkColor,
        style = StrokeStyle(width = width, cap = StrokeCap.Round, join = StrokeJoin.Round),
    )
}
