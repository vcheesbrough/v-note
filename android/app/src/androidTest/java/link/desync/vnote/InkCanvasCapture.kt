package link.desync.vnote

import androidx.compose.ui.graphics.PixelMap
import androidx.compose.ui.graphics.toPixelMap
import androidx.compose.ui.test.captureToImage
import androidx.compose.ui.test.junit4.ComposeTestRule
import androidx.compose.ui.test.onNodeWithTag

/**
 * Waits for a clean capture of the `ink-canvas` node that also satisfies
 * [condition], and returns **that** pixel map.
 *
 * The canvas is `SurfaceView`-backed, so `captureToImage()` goes through
 * `PixelCopy`, which throws `AssertionError: Failed waiting for PixelCopy!`
 * when the copy does not land in time. That is a timing failure, not a pixel
 * result, and on a cold emulator it lands often enough to flake the suite
 * (#356). Retrying is the only defence; a bare `captureToImage()` has none.
 *
 * Returning the accepted bitmap is what lets every read go through here.
 * The pattern this replaces captured once inside a `waitUntil` — where the
 * failure was swallowed and retried — and then again, unguarded, on the line
 * after it, so the capture the assertions actually read was the one with no
 * retry behind it. Assertions now read the frame the wait accepted, which is
 * both retried and, where a test asserts several things, a single consistent
 * frame rather than several successive ones.
 */
internal fun ComposeTestRule.awaitInkPixels(
    timeoutMillis: Long = 5_000,
    condition: (PixelMap) -> Boolean = { true },
): PixelMap {
    var captured: PixelMap? = null
    waitUntil(timeoutMillis) {
        runCatching {
            val pixels = onNodeWithTag("ink-canvas").captureToImage().toPixelMap()
            captured = pixels
            condition(pixels)
        }.getOrDefault(false)
    }
    return checkNotNull(captured) { "waitUntil accepted an ink-canvas capture but none was recorded" }
}
