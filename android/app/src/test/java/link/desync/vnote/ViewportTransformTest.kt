package link.desync.vnote

import androidx.compose.ui.geometry.Offset
import link.desync.vnote.ink.ViewportGestureTracker
import link.desync.vnote.ink.ViewportTransform
import link.desync.vnote.ink.applyGesture
import link.desync.vnote.ink.capMomentumVelocity
import link.desync.vnote.ink.dampVelocity
import link.desync.vnote.ink.pan
import link.desync.vnote.ink.shouldContinueMomentum
import link.desync.vnote.ink.toWorld
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class ViewportTransformTest {
    @Test
    fun zoomAroundCentroidKeepsWorldPointAnchored() {
        val initial = ViewportTransform(scale = 1.5f, offset = Offset(40f, -20f))
        val centroid = Offset(300f, 180f)
        val worldBefore = initial.toWorld(centroid)

        val zoomed = initial.applyGesture(panDelta = Offset.Zero, zoom = 1.8f, centroid = centroid)

        assertOffsetEquals(worldBefore, zoomed.toWorld(centroid))
    }

    @Test
    fun panChangesOffsetWithoutChangingScale() {
        val initial = ViewportTransform(scale = 2f, offset = Offset(10f, 20f))

        val panned = initial.pan(Offset(8f, -6f))

        assertEquals(2f, panned.scale, 0.0001f)
        assertOffsetEquals(Offset(18f, 14f), panned.offset)
    }

    @Test
    fun worldConversionUsesCurrentPanAndZoom() {
        val viewport = ViewportTransform(scale = 2f, offset = Offset(50f, -10f))

        assertOffsetEquals(Offset(25f, 55f), viewport.toWorld(Offset(100f, 100f)))
    }

    @Test
    fun momentumVelocityDecaysSlowly() {
        var velocity = Offset(900f, 0f)
        velocity = dampVelocity(velocity, deltaSeconds = 0.016f)
        val afterFirstFrame = velocity.getDistance()

        repeat(80) {
            velocity = dampVelocity(velocity, deltaSeconds = 0.016f)
        }
        val afterShortGlide = velocity.getDistance()

        repeat(95) {
            velocity = dampVelocity(velocity, deltaSeconds = 0.016f)
        }
        val afterLongerGlide = velocity.getDistance()

        assertTrue(afterFirstFrame in 899f..900f)
        assertTrue(afterShortGlide in 898f..900f)
        assertTrue(afterLongerGlide in 897f..900f)
        assertFalse(shouldContinueMomentum(Offset(10f, 0f)))
    }

    @Test
    fun momentumVelocityIsCappedForFastFlicks() {
        assertEquals(1_400f, capMomentumVelocity(Offset(3_000f, 0f)).getDistance(), 0.0001f)
    }

    @Test
    fun pointerCountChangesDoNotReusePreviousPanOrSpread() {
        val tracker = ViewportGestureTracker()

        tracker.update(pointerCount = 1, centroid = Offset(100f, 100f), spread = 0f, eventTimeMillis = 0)
        val pan = tracker.update(pointerCount = 1, centroid = Offset(130f, 100f), spread = 0f, eventTimeMillis = 16)
        val firstPinch =
            tracker.update(pointerCount = 2, centroid = Offset(150f, 100f), spread = 40f, eventTimeMillis = 32)
        val pinch =
            tracker.update(pointerCount = 2, centroid = Offset(150f, 100f), spread = 60f, eventTimeMillis = 48)
        val backToPan =
            tracker.update(pointerCount = 1, centroid = Offset(170f, 100f), spread = 0f, eventTimeMillis = 64)

        assertOffsetEquals(Offset(30f, 0f), pan.panDelta)
        assertOffsetEquals(Offset.Zero, firstPinch.panDelta)
        assertEquals(1f, firstPinch.zoom, 0.0001f)
        assertEquals(1.5f, pinch.zoom, 0.0001f)
        assertOffsetEquals(Offset.Zero, backToPan.panDelta)
        assertEquals(1f, backToPan.zoom, 0.0001f)
    }

    private fun assertOffsetEquals(
        expected: Offset,
        actual: Offset,
    ) {
        assertEquals(expected.x, actual.x, 0.0001f)
        assertEquals(expected.y, actual.y, 0.0001f)
    }
}
