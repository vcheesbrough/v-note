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
        assertEquals(3, fixture.getInt("protocol_version"))
    }

    @Test
    fun healthFixtureMatchesExpectedShape() {
        val fixture = readFixture("health.json")
        assertEquals("ok", fixture.getString("status"))
    }

    @Test
    fun pressureStrokeFixtureCarriesV2StyleAndOptionalPressure() {
        val fixture = readFixture("stroke-pressure.json")
        assertEquals(2, fixture.getJSONObject("style").getInt("style_version"))
        val points = fixture.getJSONArray("points")
        assertEquals(4, points.length())
        // Per-point pressure is present and normalised on the sampled points.
        assertEquals(0.0, points.getJSONObject(0).getDouble("pressure"), 1e-9)
        assertEquals(1.0, points.getJSONObject(2).getDouble("pressure"), 1e-9)
        // A v2 point may omit pressure entirely (renders at full width).
        assertTrue(
            "trailing point omits pressure",
            !points.getJSONObject(3).has("pressure"),
        )
    }

    private fun readFixture(name: String): JSONObject {
        val path = File(System.getProperty("user.dir")).parentFile.parentFile
        val file = File(path, "contracts/fixtures/$name")
        assertTrue("fixture exists: ${file.absolutePath}", file.exists())
        return JSONObject(file.readText())
    }
}
