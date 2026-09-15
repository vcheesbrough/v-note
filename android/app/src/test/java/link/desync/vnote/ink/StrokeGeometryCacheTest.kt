package link.desync.vnote.ink

import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import link.desync.vnote.model.SOLID_ROUND_PRESSURE_STYLE_VERSION
import link.desync.vnote.model.SolidRoundParameters
import link.desync.vnote.model.Stroke
import link.desync.vnote.model.StrokePoint
import link.desync.vnote.model.StrokeStyle
import org.junit.Assert.assertEquals
import org.junit.Test

/**
 * The cache exists so committed ink is not rebuilt every frame, so what matters
 * is *how often it builds*. Geometry construction is injected because the real
 * one allocates an `android.graphics.Path`, which is unavailable on the JVM; the
 * rasterized output is covered by the instrumented pixel tests instead.
 */
class StrokeGeometryCacheTest {
    private var builds = 0

    private val cache =
        StrokeGeometryCache { stroke ->
            builds += 1
            RenderedInk(
                InkGeometry.Dot(Offset(stroke.points[0].x.toFloat(), stroke.points[0].y.toFloat()), 1f),
                Color.Black,
            )
        }

    @Test
    fun buildsEachStrokeOnceAcrossFrames() {
        val strokes = listOf(stroke("a"), stroke("b"))

        repeat(10) { drawn(strokes) }

        assertEquals("one build per stroke, not per frame", 2, builds)
        assertEquals(2, cache.size)
    }

    @Test
    fun replaysEveryStrokeOnEveryFrame() {
        val strokes = listOf(stroke("a"), stroke("b"), stroke("c"))

        val first = drawn(strokes)
        val second = drawn(strokes)

        assertEquals(3, first.size)
        assertEquals("a cache hit still draws", first, second)
    }

    @Test
    fun rebuildsWhenTheStyleChanges() {
        drawn(listOf(stroke("a")))

        drawn(listOf(stroke("a", style = pressureStyle())))

        assertEquals(2, builds)
    }

    @Test
    fun rebuildsWhenAnIdGainsPoints() {
        drawn(listOf(stroke("a")))

        drawn(listOf(stroke("a", points = listOf(point(1.0), point(2.0)))))

        assertEquals(2, builds)
    }

    @Test
    fun evictsErasedStrokes() {
        drawn(listOf(stroke("a"), stroke("b")))

        drawn(listOf(stroke("a")))

        assertEquals("the erased stroke's geometry is released", 1, cache.size)
    }

    @Test
    fun evictsWhenAnEraseAndAnAddLeaveTheCountUnchanged() {
        drawn(listOf(stroke("a"), stroke("b")))

        drawn(listOf(stroke("a"), stroke("c")))

        assertEquals(2, cache.size)
        // `b` is gone rather than lingering keyed by a dead id: only `c` was new.
        assertEquals(3, builds)
        // A further frame of the same page rebuilds nothing.
        drawn(listOf(stroke("a"), stroke("c")))
        assertEquals(3, builds)
    }

    @Test
    fun emptyPageReleasesEverything() {
        drawn(listOf(stroke("a"), stroke("b")))

        drawn(emptyList())

        assertEquals(0, cache.size)
    }

    @Test
    fun skipsStrokesWithNoGeometry() {
        val blank = StrokeGeometryCache { null }
        var visits = 0

        blank.visitRenderable(listOf(stroke("a"))) { _, _ -> visits += 1 }

        assertEquals(0, visits)
    }

    // The geometry visited for [strokes], in draw order.
    private fun drawn(strokes: List<Stroke>): List<InkGeometry> =
        buildList { cache.visitRenderable(strokes) { geometry, _ -> add(geometry) } }

    private fun stroke(
        id: String,
        style: StrokeStyle = StrokeStyle(),
        points: List<StrokePoint> = listOf(point(1.0)),
    ) = Stroke(id = id, style = style, points = points)

    private fun point(x: Double) = StrokePoint(x = x, y = 0.0, t = 0)

    private fun pressureStyle() =
        StrokeStyle(
            styleVersion = SOLID_ROUND_PRESSURE_STYLE_VERSION,
            parameters = SolidRoundParameters(width = 8.0),
        )
}
