package link.desync.vnote.ink

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

// #390: a pen landing while a finger or palm is already down must draw, and
// the hand must not keep panning the page under it.
class CanvasPointerRouterTest {
    @Test
    fun aStylusLandingWhileAFingerPansTakesTheStream() {
        val router = CanvasPointerRouter()
        assertEquals(PointerRole.Finger, router.down(pointerId = 0, isStylus = false))
        assertEquals(PointerRole.Stylus, router.down(pointerId = 1, isStylus = true))
        assertEquals(1, router.stylusPointerId)
        assertEquals("the finger stops panning", PointerRole.Ignored, router.roleOf(0))
    }

    @Test
    fun fingersAreIgnoredOnceAStylusHasTouchedUntilEveryPointerIsUp() {
        val router = CanvasPointerRouter()
        router.down(pointerId = 0, isStylus = true)
        assertEquals("palm while drawing", PointerRole.Ignored, router.down(pointerId = 1, isStylus = false))
        assertEquals(PointerRole.Stylus, router.up(0))
        assertNull(router.stylusPointerId)
        assertEquals("the resting hand still does not pan", PointerRole.Ignored, router.down(pointerId = 2, isStylus = false))

        router.reset()
        assertEquals("a fresh stream pans again", PointerRole.Finger, router.down(pointerId = 0, isStylus = false))
    }

    @Test
    fun aSecondStrokeWhileThePalmRestsDrawsAgain() {
        val router = CanvasPointerRouter()
        router.down(pointerId = 0, isStylus = false)
        router.down(pointerId = 1, isStylus = true)
        router.up(1)
        assertEquals(PointerRole.Stylus, router.down(pointerId = 2, isStylus = true))
    }

    @Test
    fun aSecondStylusContactWhileOneDrawsIsIgnored() {
        val router = CanvasPointerRouter()
        router.down(pointerId = 0, isStylus = true)
        assertEquals(PointerRole.Ignored, router.down(pointerId = 1, isStylus = true))
        assertEquals(0, router.stylusPointerId)
    }

    @Test
    fun fingersPanTogetherAndLeaveOneByOne() {
        val router = CanvasPointerRouter()
        router.down(pointerId = 0, isStylus = false)
        assertEquals(PointerRole.Finger, router.down(pointerId = 1, isStylus = false))
        assertEquals(PointerRole.Finger, router.up(0))
        assertEquals(PointerRole.Ignored, router.roleOf(0))
        assertEquals(PointerRole.Finger, router.roleOf(1))
    }

    @Test
    fun aTapIsTheDismissalOnlyWithinTheLongPressWindow() {
        assertTrue(isSameTapAsDismissal(elapsedSinceDismissMillis = 0, tapWindowMillis = 400))
        assertTrue(isSameTapAsDismissal(elapsedSinceDismissMillis = 120, tapWindowMillis = 400))
        assertFalse(isSameTapAsDismissal(elapsedSinceDismissMillis = 401, tapWindowMillis = 400))
        // Never dismissed: uptime minus Long.MIN_VALUE overflows negative.
        assertFalse(isSameTapAsDismissal(elapsedSinceDismissMillis = 5_000L - Long.MIN_VALUE, tapWindowMillis = 400))
    }
}
