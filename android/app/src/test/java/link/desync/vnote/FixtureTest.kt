package link.desync.vnote

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.File

class FixtureTest {
    @Test
    fun metaFixtureMatchesExpectedShape() {
        val fixture = readFixture("meta.json")
        assertEquals("v-note", fixture.getString("app_name"))
        assertTrue("app_version present", fixture.getString("app_version").isNotEmpty())
        assertEquals(2, fixture.getInt("protocol_version"))
    }

    @Test
    fun healthFixtureMatchesExpectedShape() {
        val fixture = readFixture("health.json")
        assertEquals("ok", fixture.getString("status"))
    }

    private fun readFixture(name: String): JSONObject {
        val path = File(System.getProperty("user.dir")).parentFile.parentFile
        val file = File(path, "contracts/fixtures/$name")
        assertTrue("fixture exists: ${file.absolutePath}", file.exists())
        return JSONObject(file.readText())
    }
}
