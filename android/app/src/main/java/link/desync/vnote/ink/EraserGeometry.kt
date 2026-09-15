package link.desync.vnote.ink

import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.runtime.getValue
import androidx.compose.runtime.setValue
import link.desync.vnote.model.Stroke
import link.desync.vnote.model.StrokePoint
import kotlin.math.abs
import kotlin.math.min
import kotlin.math.sqrt

// Eraser hit-testing: which strokes an eraser sweep of a given radius touches.

internal fun eraseAtPoint(
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
        }.mapTo(linkedSetOf()) { it.id }
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
