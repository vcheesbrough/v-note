import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertThrows
import org.junit.Test

class WorkspaceVersionTest {
    private val cargoToml =
        """
        [workspace]
        resolver = "3"
        members = ["crates/protocol"]

        [workspace.package]
        version = "0.36.0"
        edition = "2024"

        [workspace.dependencies]
        serde = { version = "1" }
        """.trimIndent()

    @Test
    fun cargoVersionReadsWorkspacePackageVersion() {
        assertEquals("0.36.0", WorkspaceVersion.cargoVersion(cargoToml))
    }

    @Test
    fun cargoVersionTakesFirstTopLevelVersionLine() {
        val toml = "[package]\nversion = \"1.2.3\"\n\n[other]\nversion = \"9.9.9\"\n"
        assertEquals("1.2.3", WorkspaceVersion.cargoVersion(toml))
    }

    @Test
    fun cargoVersionIgnoresIndentedAndInlineVersions() {
        val toml = "  version = \"9.9.9\"\ndep = { version = \"8.8.8\" }\n"
        assertNull(WorkspaceVersion.cargoVersion(toml))
    }

    @Test
    fun cargoVersionIsNullWithoutAVersionLine() {
        assertNull(WorkspaceVersion.cargoVersion("[workspace]\nresolver = \"3\"\n"))
        assertNull(WorkspaceVersion.cargoVersion("version = \"\"\n"))
    }

    @Test
    fun versionNameUsesCargoWhenNoReleaseIsInjected() {
        assertEquals("0.36.0", WorkspaceVersion.versionName(null, cargoToml))
        assertEquals("0.36.0", WorkspaceVersion.versionName("  ", cargoToml))
    }

    @Test
    fun injectedReleaseWinsOverCargo() {
        assertEquals("0.36.4", WorkspaceVersion.versionName(" 0.36.4\n", cargoToml))
        assertEquals("0.36.4", WorkspaceVersion.versionName("0.36.4", null))
    }

    @Test
    fun versionNameFailsLoudlyWithoutAnySource() {
        assertThrows(IllegalStateException::class.java) {
            WorkspaceVersion.versionName(null, null)
        }
        assertThrows(IllegalStateException::class.java) {
            WorkspaceVersion.versionName("", "[workspace]\n")
        }
    }

    @Test
    fun versionCodeFollowsMajorMinorPatchFormula() {
        assertEquals(36_000, WorkspaceVersion.versionCode("0.36.0"))
        assertEquals(4_001, WorkspaceVersion.versionCode("0.4.1"))
        assertEquals(1_002_003, WorkspaceVersion.versionCode("1.2.3"))
        assertEquals(36_002, WorkspaceVersion.versionCode("0.36.2-rc.1"))
        assertEquals(1, WorkspaceVersion.versionCode("0.0.0"))
    }
}
