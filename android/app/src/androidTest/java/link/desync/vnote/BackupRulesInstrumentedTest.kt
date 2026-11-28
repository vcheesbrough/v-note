package link.desync.vnote

import android.content.pm.ApplicationInfo
import android.content.res.XmlResourceParser
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.xmlpull.v1.XmlPullParser

/**
 * #159: app data never leaves the device except to the v-note server. Read
 * from the installed app — the merged manifest, as the platform sees it —
 * so a library that re-enables backup through manifest merging fails here.
 */
@RunWith(AndroidJUnit4::class)
class BackupRulesInstrumentedTest {
    private val context = InstrumentationRegistry.getInstrumentation().targetContext
    private val info: ApplicationInfo = context.packageManager.getApplicationInfo(context.packageName, 0)

    @Test
    fun backupIsOff() {
        assertEquals("allowBackup is off", 0, info.flags and ApplicationInfo.FLAG_ALLOW_BACKUP)
    }

    /**
     * Android 12+ reads `dataExtractionRules`, and there `allowBackup="false"`
     * alone does not stop device-to-device transfer: every domain must be
     * excluded from cloud backup and from device transfer. Read off the
     * installed APK's own (merged) manifest.
     */
    @Test
    fun everyDomainIsExcludedFromCloudBackupAndDeviceTransfer() {
        val rules = applicationAttribute("dataExtractionRules")
        assertNotEquals("the merged manifest names extraction rules", 0, rules)

        val excluded = excludedDomains(context.resources.getXml(rules))

        for (section in listOf("cloud-backup", "device-transfer")) {
            assertEquals(
                "$section excludes every domain",
                setOf("root", "file", "database", "sharedpref", "external"),
                excluded[section],
            )
        }
    }

    /** Android 11 and below: the full-backup rules exclude everything too. */
    @Test
    fun theFullBackupRulesExcludeEverything() {
        val rules = applicationAttribute("fullBackupContent")
        assertNotEquals("the merged manifest names full-backup rules", 0, rules)
        val parser = context.resources.getXml(rules)
        val excluded = mutableSetOf<String>()
        while (parser.next() != XmlPullParser.END_DOCUMENT) {
            if (parser.eventType != XmlPullParser.START_TAG) continue
            assertTrue("only exclusions: found <${parser.name}>", parser.name in setOf("full-backup-content", "exclude"))
            if (parser.name == "exclude") excluded += parser.getAttributeValue(null, "domain")
        }
        assertEquals(setOf("root", "file", "database", "sharedpref", "external"), excluded)
    }

    // A resource-valued attribute of `<application>` in the installed APK's
    // binary manifest, or 0 when it is absent.
    private fun applicationAttribute(name: String): Int {
        val parser = context.assets.openXmlResourceParser("AndroidManifest.xml")
        while (parser.next() != XmlPullParser.END_DOCUMENT) {
            if (parser.eventType == XmlPullParser.START_TAG && parser.name == "application") {
                return parser.getAttributeResourceValue(ANDROID_NS, name, 0)
            }
        }
        return 0
    }

    // Section name → the domains it excludes, and nothing is included.
    private fun excludedDomains(parser: XmlResourceParser): Map<String, Set<String>> {
        val excluded = mutableMapOf<String, MutableSet<String>>()
        var section: String? = null
        while (parser.next() != XmlPullParser.END_DOCUMENT) {
            if (parser.eventType != XmlPullParser.START_TAG) continue
            when (parser.name) {
                "cloud-backup", "device-transfer" -> section = parser.name
                "exclude" -> excluded.getOrPut(section.orEmpty()) { mutableSetOf() } += parser.getAttributeValue(null, "domain")
                else -> assertTrue("only exclusions: found <${parser.name}>", parser.name == "data-extraction-rules")
            }
        }
        return excluded
    }

    private companion object {
        const val ANDROID_NS = "http://schemas.android.com/apk/res/android"
    }
}
