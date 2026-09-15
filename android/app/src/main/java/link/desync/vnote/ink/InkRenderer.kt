package link.desync.vnote.ink

import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Path
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.StrokeJoin
import androidx.compose.ui.graphics.drawscope.DrawScope
import link.desync.vnote.model.Stroke
import link.desync.vnote.model.StrokePoint
import link.desync.vnote.model.StrokeStyle
import kotlin.math.sqrt
import androidx.compose.ui.graphics.drawscope.Stroke as DrawStroke

/**
 * One stroke's rendered shape, in **world space**.
 *
 * Shape is separated from drawing so it can be built once and replayed on every
 * later frame: the canvas applies the viewport transform, so world-space
 * geometry survives pan and zoom untouched. See [StrokeGeometryCache].
 */
internal sealed interface InkGeometry {
    /** A tap, or a stroke whose extent is smaller than its own nib. */
    data class Dot(
        val center: Offset,
        val radius: Float,
    ) : InkGeometry

    /** Constant-width (v1) ink: one round-capped, round-joined polyline. */
    data class Polyline(
        val path: Path,
        val width: Float,
    ) : InkGeometry

    /** Pressure-modulated (v2) ink: one filled variable-width ribbon. */
    data class Ribbon(
        val path: Path,
    ) : InkGeometry
}

/**
 * Build the world-space geometry for [points] under [style], or null when there
 * is nothing to draw.
 *
 * The branches and their arithmetic are the pre-cache renderer's, unchanged, so
 * the rasterized result stays identical to the SPA and thumbnails.
 */
internal fun buildInkGeometry(
    points: List<StrokePoint>,
    style: StrokeStyle,
): InkGeometry? {
    if (points.isEmpty()) {
        return null
    }
    if (points.size == 1) {
        return InkGeometry.Dot(
            center = Offset(points[0].x.toFloat(), points[0].y.toFloat()),
            radius = (style.renderedWidth(points[0].pressure) / 2.0).toFloat(),
        )
    }
    // Constant-width (v1) ink: a single round-capped polyline. Byte-identical to
    // the pre-pressure renderer.
    if (!style.isPressureSensitive) {
        return InkGeometry.Polyline(
            path = polylinePath(points),
            width = style.parameters.width.toFloat(),
        )
    }
    // A tap/dot commits (near-)coincident points; the ribbon would collapse to a
    // zero-area sliver and vanish. If the stroke's extent is smaller than its own
    // nib, render a dot at the largest pressure width (v1 drew these via round
    // caps).
    val minX = points.minOf { it.x }
    val maxX = points.maxOf { it.x }
    val minY = points.minOf { it.y }
    val maxY = points.maxOf { it.y }
    val maxWidth = points.maxOf { style.renderedWidth(it.pressure) }
    if (maxOf(maxX - minX, maxY - minY) < maxWidth) {
        return InkGeometry.Dot(
            center = Offset(((minX + maxX) / 2.0).toFloat(), ((minY + maxY) / 2.0).toFloat()),
            radius = (maxWidth / 2.0).toFloat(),
        )
    }
    // Pressure-modulated (v2) ink: one filled variable-width ribbon, drawn in a
    // single call. Per-segment stroking was O(points) draw calls per stroke,
    // re-run for every committed stroke every frame — the source of the
    // multi-stroke latency. A single fill restores ~constant-width cost.
    return InkGeometry.Ribbon(buildPressureRibbon(points, style))
}

internal fun DrawScope.drawInkGeometry(
    geometry: InkGeometry,
    color: Color,
) {
    when (geometry) {
        is InkGeometry.Dot ->
            drawCircle(
                color = color,
                radius = geometry.radius,
                center = geometry.center,
            )
        is InkGeometry.Polyline ->
            drawPath(
                path = geometry.path,
                color = color,
                style =
                    DrawStroke(
                        width = geometry.width,
                        cap = StrokeCap.Round,
                        join = StrokeJoin.Round,
                    ),
            )
        is InkGeometry.Ribbon -> drawPath(path = geometry.path, color = color)
    }
}

/** Build and draw in one step. For ink that is not worth caching. */
internal fun DrawScope.drawInk(
    points: List<StrokePoint>,
    style: StrokeStyle,
    color: Color,
) {
    buildInkGeometry(points, style)?.let { drawInkGeometry(it, color) }
}

private fun polylinePath(points: List<StrokePoint>): Path {
    val path = Path()
    path.moveTo(points[0].x.toFloat(), points[0].y.toFloat())
    for (index in 1 until points.size) {
        path.lineTo(points[index].x.toFloat(), points[index].y.toFloat())
    }
    return path
}

// One filled polygon approximating a variable-width stroke: walk the left offset
// forward, then the right offset back, and close (flat end caps). Per-vertex
// averaged normals keep joins smooth. Filled once (NonZero), this replaces the
// O(points) per-segment stroking that scaled badly across many strokes.
private fun buildPressureRibbon(
    points: List<StrokePoint>,
    style: StrokeStyle,
): Path {
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

/** Everything needed to draw one stroke, derived once and then reused. */
internal data class RenderedInk(
    val geometry: InkGeometry,
    val color: Color,
)

internal fun renderedInk(stroke: Stroke): RenderedInk? =
    buildInkGeometry(stroke.points, stroke.style)?.let { geometry ->
        RenderedInk(geometry, parseColor(stroke.style.parameters.color))
    }

/**
 * Per-stroke [RenderedInk], keyed by stroke id.
 *
 * Committed ink is redrawn on every frame of every pan, zoom and live stylus
 * sample. Rebuilding each stroke's `Path` — and, for pressure ink, its whole
 * offset-normal ribbon — plus re-parsing its colour hex made the per-frame cost
 * O(points on the page), which is what put the nib ahead of its own ink on a
 * dense page. Geometry is world-space, so one build per stroke serves every
 * later frame, at any pan or zoom.
 *
 * Deliberately **not** Compose state: it is read and written during the draw
 * phase, where a write must never schedule a recomposition.
 *
 * [render] is injectable so the caching and eviction rules can be unit-tested
 * without `android.graphics`.
 */
internal class StrokeGeometryCache(
    private val render: (Stroke) -> RenderedInk? = ::renderedInk,
) {
    private class Entry(
        val style: StrokeStyle,
        val pointCount: Int,
        val ink: RenderedInk?,
    ) {
        var usedAt: Long = 0
    }

    private val entries = HashMap<String, Entry>()
    private var generation = 0L

    internal val size: Int get() = entries.size

    /**
     * Visit every stroke's geometry in draw order, building what is missing and
     * dropping entries for strokes that have left the page.
     */
    fun visitRenderable(
        strokes: List<Stroke>,
        visit: (InkGeometry, Color) -> Unit,
    ) {
        generation += 1
        for (stroke in strokes) {
            val cached = entries[stroke.id]
            // The point count guards against an id whose geometry grew — a
            // stroke is immutable once committed, but an optimistic batch and
            // its server echo share an id, so the cheap check earns its keep.
            val entry =
                if (cached != null &&
                    cached.pointCount == stroke.points.size &&
                    cached.style == stroke.style
                ) {
                    cached
                } else {
                    Entry(
                        style = stroke.style,
                        pointCount = stroke.points.size,
                        ink = render(stroke),
                    ).also { entries[stroke.id] = it }
                }
            entry.usedAt = generation
            entry.ink?.let { visit(it.geometry, it.color) }
        }
        // Only scan when the map has outgrown the page. An erase leaves it
        // oversized; so does an erase plus an add in the same frame, because the
        // added stroke is inserted before this check runs.
        if (entries.size > strokes.size) {
            entries.values.retainAll { it.usedAt == generation }
        }
    }
}

// The live stroke is deliberately rebuilt in full on every frame rather than
// extended in place. Appending to a retained path only helps constant-width
// (v1) ink, and the shipped pen is pressure-sensitive (v2, see
// `DrawingToolPreferences.load`), whose ribbon cannot be extended at all — a new
// sample changes the previous vertex's averaged normal. So an incremental
// builder would be dead code for every stroke a user actually draws, while the
// live layer already bounds the cost at one stroke per frame instead of the
// page's. Revisit only if a device trace shows a long v2 stroke missing frames.
