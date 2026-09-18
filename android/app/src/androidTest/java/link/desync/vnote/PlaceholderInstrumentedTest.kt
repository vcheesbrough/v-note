package link.desync.vnote

import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class PlaceholderInstrumentedTest {
    @get:Rule
    val composeRule = createAndroidComposeRule<MainActivity>()

    /**
     * Signed out, the app shows the library with no pages in it and a way back
     * in (#317) — the same top bar the signed-in library wears, not a separate
     * auth screen. The create control belongs to a session, so it is absent.
     */
    @Test
    fun launchesTheEmptyLibraryWhenSignedOut() {
        composeRule.onNodeWithText("v-note").assertIsDisplayed()
        composeRule.onNodeWithContentDescription("Open main menu").assertIsDisplayed()
        composeRule.waitUntil(timeoutMillis = 15_000) {
            runCatching {
                composeRule.onNodeWithText("Sign in").assertIsDisplayed()
                true
            }.getOrDefault(false)
        }
        composeRule.onNodeWithTag("create-page-button").assertDoesNotExist()
        composeRule
            .onNodeWithText("Use Authentik to access your page library and ink canvas.")
            .assertIsDisplayed()
        composeRule
            .onNodeWithTag("version-watermark")
            .assertIsDisplayed()
        composeRule
            .onNodeWithText("v${BuildConfig.VERSION_NAME} · ${BuildConfig.FLAVOR}")
            .assertIsDisplayed()
    }
}
