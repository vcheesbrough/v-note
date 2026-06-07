package link.desync.vnote

import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onAllNodesWithText
import androidx.compose.ui.test.onNodeWithText
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class PlaceholderInstrumentedTest {
    @get:Rule
    val composeRule = createAndroidComposeRule<MainActivity>()

    @Test
    fun launchesAuthShellWhenSignedOut() {
        composeRule.onNodeWithText("v-note").assertIsDisplayed()
        composeRule.waitUntil(timeoutMillis = 15_000) {
            composeRule.onAllNodesWithText("Sign in").fetchSemanticsNodes().isNotEmpty()
        }
        composeRule
            .onNodeWithText("Sign in with Authentik to use v-note on this device.")
            .assertIsDisplayed()
    }
}
