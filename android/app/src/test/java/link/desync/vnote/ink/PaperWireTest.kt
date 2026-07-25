package link.desync.vnote.ink

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.File

/**
 * The paper wire vocabulary, asserted against the same contract fixtures the
 * Rust `contracts` test suite parses. Anything the server can send, this client
 * must be able to name.
 */
class PaperWireTest {
    @Test
    fun everyPaperCarriedByAFixtureParses() {
        // (fixture, JSON path to the paper value)
        val cases =
            listOf(
                "page.json" to listOf<String>(),
                "create-page.json" to listOf(),
                "page-server-welcome.json" to listOf(),
                "page-client-set-paper.json" to listOf(),
                "page-server-paper-changed.json" to listOf(),
                "library-event-page-created.json" to listOf("page"),
            )
        for ((name, path) in cases) {
            var json = readFixture(name)
            path.forEach { json = json.getJSONObject(it) }
            val wire = json.getString("paper")
            assertNotNull("$name carries an unknown paper: $wire", Paper.fromWire(wire))
        }

        // `pages.json` nests its page inside an array.
        val listed = readFixture("pages.json").getJSONArray("pages").getJSONObject(0)
        assertNotNull(Paper.fromWire(listed.getString("paper")))
    }

    @Test
    fun setPaperFixtureUsesTheExpectedShape() {
        val json = readFixture("page-client-set-paper.json")
        assertEquals("set-paper", json.getString("type"))
        assertTrue(json.getString("client_mutation_id").isNotEmpty())
        assertEquals(Paper.RuledMarginNarrow, Paper.fromWire(json.getString("paper")))
    }

    @Test
    fun paperChangedFixtureUsesTheExpectedShape() {
        val json = readFixture("page-server-paper-changed.json")
        assertEquals("paper-changed", json.getString("type"))
        assertEquals(Paper.RuledMarginNarrow, Paper.fromWire(json.getString("paper")))
        assertTrue(json.getLong("revision") >= 0)
    }

    /** A pre-v5 payload carries no paper at all and must read as a blank page. */
    @Test
    fun legacyPayloadsWithoutPaperReadAsNone() {
        val legacy = JSONObject("""{"id":"page_1","title":"t"}""")
        assertEquals(Paper.None, Paper.fromWire(legacy.optString("paper").ifBlank { null }) ?: Paper.None)
    }

    /** An unknown value is rejected, never silently treated as "no paper". */
    @Test
    fun unknownPaperValuesAreRejected() {
        assertNull(Paper.fromWire("ruled-margin-huge"))
        assertNull(Paper.fromWire("NONE"))
        assertNull(Paper.fromWire("ruled_narrow"))
    }

    private fun readFixture(name: String): JSONObject {
        val root = File(System.getProperty("user.dir")).parentFile.parentFile
        val file = File(root, "contracts/fixtures/$name")
        assertTrue("fixture exists: ${file.absolutePath}", file.exists())
        return JSONObject(file.readText())
    }
}
