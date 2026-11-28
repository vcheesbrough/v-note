package link.desync.vnote

import android.os.Bundle
import androidx.test.platform.app.InstrumentationRegistry
import androidx.test.runner.AndroidJUnitRunner
import link.desync.vnote.store.LocalStore
import org.junit.runner.Description
import org.junit.runner.notification.RunListener

/**
 * The instrumentation runner for every `androidTest` run (#464). It only adds
 * [TestNetwork] to the stock runner.
 *
 * `onCreate` runs after the Application object is constructed but **before**
 * `Application.onCreate`, so the telemetry exporter [VNoteApplication] installs
 * there already points at [TestNetwork.SERVER_BASE_URL], and every HTTP client
 * the app builds carries the loopback-only guard.
 */
class VNoteTestRunner : AndroidJUnitRunner() {
    override fun onCreate(arguments: Bundle?) {
        TestNetwork.install()
        // Every test starts from an empty device copy (#159): the store is
        // one file per process, and every test runs in this one.
        val args = arguments ?: Bundle()
        val listeners = listOfNotNull(args.getString("listener"), FreshLocalStore::class.java.name)
        args.putString("listener", listeners.joinToString(","))
        super.onCreate(args)
    }
}

/**
 * Empties the app's local-first store (#159) before each test, so no test
 * inherits another's library, ink, outbox or stored identity.
 */
class FreshLocalStore : RunListener() {
    override fun testStarted(description: Description) {
        LocalStore.shared(InstrumentationRegistry.getInstrumentation().targetContext).resetAll()
    }
}
