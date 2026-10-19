package link.desync.vnote

import android.os.Bundle
import androidx.test.runner.AndroidJUnitRunner

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
        super.onCreate(arguments)
    }
}
