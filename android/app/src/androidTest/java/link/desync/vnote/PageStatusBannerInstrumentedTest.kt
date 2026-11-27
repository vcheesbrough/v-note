package link.desync.vnote

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.test.assertCountEquals
import androidx.compose.ui.test.assertTextEquals
import androidx.compose.ui.test.junit4.v2.createComposeRule
import androidx.compose.ui.test.onAllNodesWithTag
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.performClick
import androidx.test.ext.junit.runners.AndroidJUnit4
import link.desync.vnote.ink.PageStatus
import link.desync.vnote.ink.PageStatusBanner
import link.desync.vnote.ink.PageStatusKind
import org.junit.Assert.assertEquals
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith

/**
 * #150: the page banner on its own — the held-ink count and each kind's
 * action. [PageSyncBannerInstrumentedTest] drives the same banner from a live
 * page channel.
 */
@RunWith(AndroidJUnit4::class)
class PageStatusBannerInstrumentedTest {
    @get:Rule
    val composeRule = createComposeRule()

    private var retries = 0
    private var signIns = 0

    private fun show(
        status: PageStatus,
        unsent: Int,
    ) {
        composeRule.setContent {
            PageStatusBanner(
                status = status,
                unsentStrokes = unsent,
                onRetry = { retries += 1 },
                onSignIn = { signIns += 1 },
            )
        }
    }

    @Test
    fun heldInkIsCountedWhileDisconnected() {
        var unsent by mutableStateOf(1)
        composeRule.setContent {
            PageStatusBanner(
                status = PageStatus(PageStatusKind.Disconnected, "Disconnected", detail = "connection closed"),
                unsentStrokes = unsent,
                onRetry = {},
                onSignIn = {},
            )
        }
        composeRule.onNodeWithTag("page-status-unsent").assertTextEquals("1 stroke not yet saved")

        unsent = 3
        composeRule.onNodeWithTag("page-status-unsent").assertTextEquals("3 strokes not yet saved")

        // Nothing held, nothing to say.
        unsent = 0
        composeRule.onAllNodesWithTag("page-status-unsent").assertCountEquals(0)
    }

    // #630: with the edit lease gone, there is no "another device is editing"
    // kind left to show — every banner is about this device's own sync.
    @Test
    fun noBannerKindIsAboutAnotherEditor() {
        assertEquals(
            listOf("connecting", "disconnected", "retrying", "session-expired", "rejected"),
            PageStatusKind.entries.map { it.tag },
        )
    }

    @Test
    fun retryIsOfferedForADisconnect() {
        show(PageStatus(PageStatusKind.Disconnected, "Disconnected"), unsent = 0)
        composeRule.onNodeWithTag("page-status-retry").performClick()
        composeRule.runOnIdle { assertEquals(1, retries) }
        composeRule.onAllNodesWithTag("page-status-sign-in").assertCountEquals(0)
    }

    @Test
    fun signInIsOfferedForAnExpiredSession() {
        show(PageStatus(PageStatusKind.SessionExpired, "Your session has expired — sign in again"), unsent = 1)
        composeRule.onNodeWithTag("page-status-sign-in").performClick()
        composeRule.runOnIdle { assertEquals(1, signIns) }
        composeRule.onAllNodesWithTag("page-status-retry").assertCountEquals(0)
    }

    @Test
    fun retryingNeedsNoActionFromTheUser() {
        show(PageStatus(PageStatusKind.Retrying, "Couldn't save your ink — retrying"), unsent = 1)
        composeRule.onAllNodesWithTag("page-status-retry").assertCountEquals(0)
        composeRule.onAllNodesWithTag("page-status-sign-in").assertCountEquals(0)
        composeRule.onNodeWithTag("page-status-unsent").assertTextEquals("1 stroke not yet saved")
    }
}
