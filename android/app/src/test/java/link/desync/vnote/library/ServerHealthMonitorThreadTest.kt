package link.desync.vnote.library

import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.asCoroutineDispatcher
import kotlinx.coroutines.cancel
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import java.util.Collections
import java.util.concurrent.CountDownLatch
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import kotlin.coroutines.CoroutineContext
import kotlin.coroutines.EmptyCoroutineContext

/**
 * Which thread the health verdict is written on (#465).
 *
 * The monitor runs in a composition's scope and the real probe hops to
 * `Dispatchers.IO`. Under the Compose test harness that scope resumes a
 * coroutine wherever it was woken, so the verdict — and the `collectAsState`
 * that reads it — landed on an IO worker, and the instrumented suite failed
 * with "Detected multithreaded access to SnapshotStateObserver". Here a
 * single named thread stands in for the main thread, the scope is
 * `Unconfined` as the harness's is, and the collector is `Unconfined` as
 * `collectAsState`'s effectively was, so it runs on whichever thread wrote
 * the value.
 */
class ServerHealthMonitorThreadTest {
    private val uiExecutor = Executors.newSingleThreadExecutor { Thread(it, UI_THREAD) }
    private val ui = uiExecutor.asCoroutineDispatcher()
    private val scope = CoroutineScope(Job() + Dispatchers.Unconfined)

    @After
    fun tearDown() {
        scope.cancel()
        ui.close()
    }

    @Test
    fun theVerdictIsWrittenOnTheStateContextAfterAnIoProbe() {
        val writes = verdictThreads(stateContext = ui, check = { ioCheck(Result.success(Unit)) })

        assertEquals(listOf(UI_THREAD), writes)
    }

    @Test
    fun aFailureIsReportedAndWrittenOnTheStateContext() {
        val givenUpOn = Collections.synchronizedList(mutableListOf<String>())

        val writes =
            verdictThreads(
                stateContext = ui,
                check = { ioCheck(Result.failure(IllegalStateException("HTTP 404"))) },
                onGiveUp = { givenUpOn += currentThreadName() },
            )

        assertEquals(listOf(UI_THREAD), writes)
        assertEquals(listOf(UI_THREAD), givenUpOn)
    }

    // The path #465 removed: without a pinned context the verdict is written on
    // the IO worker the probe resumed on. Kept so the test above is known to be
    // able to fail.
    @Test
    fun withoutAPinnedContextTheVerdictLandsOnTheIoWorker() {
        val writes = verdictThreads(stateContext = EmptyCoroutineContext, check = { ioCheck(Result.success(Unit)) })

        assertTrue("written on $writes", writes.single().startsWith("DefaultDispatcher-worker"))
    }

    // Coroutine debug mode, on under Gradle, appends " @coroutine#N" to the name.
    private fun currentThreadName(): String = Thread.currentThread().name.substringBefore(" @")

    private suspend fun ioCheck(result: Result<Unit>): Result<Unit> = withContext(Dispatchers.IO) { result }

    // Runs one check and returns the threads the verdict (anything past
    // Checking) was delivered on.
    private fun verdictThreads(
        stateContext: CoroutineContext,
        check: suspend () -> Result<Unit>,
        onGiveUp: (String) -> Unit = {},
    ): List<String> {
        val monitor =
            ServerHealthMonitor(
                probe = ServerHealthProbe(BASE_URL, check, retryDelaysMillis = emptyList()),
                scope = scope,
                onGiveUp = onGiveUp,
                stateContext = stateContext,
            )
        val threads = Collections.synchronizedList(mutableListOf<String>())
        val verdict = CountDownLatch(1)
        scope.launch(Dispatchers.Unconfined) {
            monitor.state.collect { health ->
                if (health !is ServerHealth.Checking) {
                    threads += currentThreadName()
                    verdict.countDown()
                }
            }
        }

        // Started from the "UI" thread, as the menu and LaunchedEffect do.
        uiExecutor.submit { monitor.check() }.get(TIMEOUT_SECONDS, TimeUnit.SECONDS)

        assertTrue("no verdict within $TIMEOUT_SECONDS s", verdict.await(TIMEOUT_SECONDS, TimeUnit.SECONDS))
        return threads.toList()
    }

    private companion object {
        const val BASE_URL = "https://v-notes-dev.example"
        const val UI_THREAD = "fake-main"
        const val TIMEOUT_SECONDS = 10L
    }
}
