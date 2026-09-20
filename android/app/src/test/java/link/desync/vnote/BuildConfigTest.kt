package link.desync.vnote

import org.junit.Assert.assertEquals
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
}
