package link.desync.vnote

import android.os.Looper
import androidx.compose.runtime.snapshots.Snapshot
import org.junit.rules.TestRule
import org.junit.runner.Description
import org.junit.runners.model.Statement
import java.util.concurrent.ConcurrentLinkedQueue

/**
 * Fails a Compose test if snapshot apply notifications are sent off the main
 * thread while it runs (#465).
 *
 * That is how "Detected multithreaded access to SnapshotStateObserver" reached
 * the suite: a composition-scoped coroutine resumed on an IO worker after
 * `withContext(Dispatchers.IO)`, the test harness's continuation interceptor
 * then called `Snapshot.sendApplyNotifications()` on that worker, and the
 * owner's observers started reading semantics there while the main thread was
 * using them. The crash itself needs the two threads to collide, so it flaked;
 * this check does not: every apply notification on any other thread is
 * recorded and fails the test, whether or not it happened to collide.
 *
 * The thread running the test is exempt. The v2 rule's teardown drains and
 * cancels the composition's test-dispatcher coroutines on it (`runTest {}` and
 * the recomposer scope's `cancel()` in `AndroidComposeUiTestEnvironment`), and
 * each of those sends an apply notification there; that is the harness, after
 * the test, with nothing left on the main thread to collide with.
 *
 * The message names the thread, the state objects that were pending (which
 * thread wrote them is not recorded) and the frames that sent the
 * notification. Put it outermost in the rule chain so activity launch and
 * teardown are covered too.
 */
class SnapshotThreadGuard : TestRule {
    override fun apply(
        base: Statement,
        description: Description,
    ): Statement =
        object : Statement() {
            override fun evaluate() {
                val violations = ConcurrentLinkedQueue<String>()
                val testThread = Thread.currentThread()
                val handle =
                    Snapshot.registerApplyObserver { changed, _ ->
                        val thread = Thread.currentThread()
                        if (Looper.myLooper() != Looper.getMainLooper() && thread != testThread) {
                            violations += describe(thread, changed)
                        }
                    }
                var testFailure: Throwable? = null
                try {
                    base.evaluate()
                } catch (failure: Throwable) {
                    testFailure = failure
                    throw failure
                } finally {
                    handle.dispose()
                    if (violations.isNotEmpty()) {
                        val error = AssertionError(report(description, violations))
                        val failure = testFailure
                        if (failure == null) throw error else failure.addSuppressed(error)
                    }
                }
            }
        }

    private fun describe(
        thread: Thread,
        changed: Set<Any>,
    ): String {
        val states = changed.take(MAX_STATES_PER_VIOLATION).joinToString { it.toString().take(MAX_STATE_CHARS) }
        val frames =
            Throwable()
                .stackTrace
                .dropWhile { it.className.startsWith(GUARD_PACKAGE_PREFIX) || it.className.startsWith(SNAPSHOT_PACKAGE) }
                .take(MAX_FRAMES)
                .joinToString("\n      at ")
        return "thread=${thread.name} changed=[$states]\n      at $frames"
    }

    private fun report(
        description: Description,
        violations: Collection<String>,
    ): String =
        "${violations.size} snapshot apply notification(s) sent off the main thread during " +
            "${description.displayName}; Compose state must be applied on the main thread:\n" +
            violations.take(MAX_REPORTED_VIOLATIONS).joinToString("\n") { "  $it" }

    private companion object {
        const val MAX_STATES_PER_VIOLATION = 5
        const val MAX_STATE_CHARS = 160
        const val MAX_REPORTED_VIOLATIONS = 10
        const val MAX_FRAMES = 12
        const val SNAPSHOT_PACKAGE = "androidx.compose.runtime.snapshots."
        val GUARD_PACKAGE_PREFIX: String = SnapshotThreadGuard::class.java.name
    }
}
