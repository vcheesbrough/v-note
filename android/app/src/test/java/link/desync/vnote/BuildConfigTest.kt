package link.desync.vnote

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class BuildConfigTest {
    @Test
    fun baseUrlMatchesFlavor() {
        val expected =
            when (BuildConfig.FLAVOR) {
                "dev" -> "https://v-notes-dev.desync.link"
                "devLocal" -> "http://127.0.0.1:8080"
                // Adding a flavor without a row here fails this test rather than
                // shipping an unasserted BASE_URL.
                else -> error("Untested flavor: ${BuildConfig.FLAVOR}")
            }
        assertEquals("BASE_URL for flavor '${BuildConfig.FLAVOR}'", expected, BuildConfig.BASE_URL)
    }

    // #439: every flavor asks for the scope the client-telemetry ingest
    // requires, and `profile` for the `preferred_username` it also requires.
    @Test
    fun scopesIncludeWhatTheTelemetryIngestRequires() {
        val scopes = BuildConfig.OIDC_SCOPES.split(' ')
        for (required in listOf("openid", "profile", "offline_access", "telemetry:write")) {
            assertTrue("$required in '${BuildConfig.OIDC_SCOPES}'", required in scopes)
        }
    }
}
