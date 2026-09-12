package link.desync.vnote

import android.util.Log
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import kotlinx.coroutines.runBlocking
import link.desync.vnote.auth.ApiClient
import link.desync.vnote.auth.AuthConfig
import link.desync.vnote.auth.AuthRepository
import link.desync.vnote.auth.PageEvent
import link.desync.vnote.auth.PageEventListener
import link.desync.vnote.auth.SOLID_ROUND_PRESSURE_STYLE_VERSION
import link.desync.vnote.auth.SolidRoundParameters
import link.desync.vnote.auth.Stroke
import link.desync.vnote.auth.StrokePoint
import link.desync.vnote.auth.StrokeStyle
import link.desync.vnote.auth.TokenStore
import link.desync.vnote.ink.Paper
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Test
import org.junit.runner.RunWith
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicReference
import kotlin.math.cos
import kotlin.math.sin
import kotlin.random.Random

/**
 * Measurement fixture for card #312 — **not** part of the test suite.
 *
 * Creates one page carrying a very large amount of ink on whatever environment
 * the installed flavor points at, so the rendering cost of a dense page can be
 * measured on a real device. Ignored by default; run it explicitly:
 *
 * ```
 * ./gradlew :app:connectedDevDebugAndroidTest \
 *   -Pandroid.testInstrumentationRunnerArguments.class=link.desync.vnote.DensePageSeeder#seedDensePage
 * ```
 *
 * It runs in the app's own process, so it reads the **real** signed-in session
 * out of the Keystore-backed [TokenStore] — and deliberately never writes to or
 * clears it, unlike the behavioural instrumented tests. Sign in normally first.
 *
 * One stroke per batch, matching what the app itself commits, so the page also
 * reproduces the replay path faithfully: the server sends one `stroke-batch`
 * message per stored batch, so N strokes means N messages on open.
 *
 * Delete the page from the library when the measurement is done.
 *
 * [MeasurementFixture] keeps it out of the CI suite; the `strokes` guard below
 * also makes it inert in any run that does not explicitly ask for it.
 */
@MeasurementFixture
@RunWith(AndroidJUnit4::class)
class DensePageSeeder {
    @Test
    fun seedDensePage() {
        val strokesArg = InstrumentationRegistry.getArguments().getString("strokes")
        assumeTrue(
            "measurement fixture — pass `-e strokes <n>` to run it",
            strokesArg != null,
        )
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        val tokenStore = TokenStore(context)
        assertTrue(
            "no signed-in session on this device — sign in to the app first",
            tokenStore.hasSession(),
        )
        val authRepository = AuthRepository(context, AuthConfig.fromBuildConfig(), tokenStore)
        val apiClient = ApiClient(BuildConfig.BASE_URL, tokenStore, authRepository)

        val strokeCount = strokesArg!!.toIntOrNull() ?: DEFAULT_STROKES
        val pointsPerStroke =
            InstrumentationRegistry.getArguments().getString("points")?.toIntOrNull()
                ?: DEFAULT_POINTS

        val page =
            runBlocking {
                apiClient.createPage(
                    title = "perf-$strokeCount-strokes",
                    paper = Paper.RuledWide,
                )
            }.getOrElse { error("createPage failed: $it") }
        Log.i(TAG, "seeding page ${page.id} with $strokeCount strokes")

        val welcomed = CountDownLatch(1)
        val leased = CountDownLatch(1)
        val acked = CountDownLatch(strokeCount)
        val failure = AtomicReference<String?>(null)

        val socket =
            apiClient.openPageSocket(
                page.id,
                object : PageEventListener {
                    override fun onEvent(event: PageEvent) {
                        when (event) {
                            is PageEvent.Welcome -> welcomed.countDown()
                            PageEvent.LeaseGranted -> leased.countDown()
                            is PageEvent.StrokeBatch -> acked.countDown()
                            is PageEvent.Failure -> failure.set("${event.code}: ${event.message}")
                            else -> Unit
                        }
                    }

                    override fun onError(message: String) {
                        failure.set(message)
                    }

                    override fun onClosed() = Unit
                },
            ) ?: error("could not open page socket")

        assertTrue("no welcome", welcomed.await(30, TimeUnit.SECONDS))
        socket.acquireLease()
        assertTrue("no lease", leased.await(30, TimeUnit.SECONDS))

        val random = Random(SEED)
        repeat(strokeCount) { index ->
            socket.commitBatch(
                "seed_${index}_${System.nanoTime()}",
                listOf(scribble(index, pointsPerStroke, random)),
            )
            // The server persists each batch; without a little back-pressure the
            // socket's send queue outruns it on a page this size.
            if (index % PAUSE_EVERY == 0) {
                Thread.sleep(PAUSE_MS)
            }
        }

        val done = acked.await(20, TimeUnit.MINUTES)
        socket.releaseLease()
        socket.close()
        failure.get()?.let { error("server rejected a batch: $it") }
        assertTrue("only ${strokeCount - acked.count} of $strokeCount batches acked", done)
        Log.i(TAG, "seeded page ${page.id}: $strokeCount strokes, ${strokeCount * pointsPerStroke} points")
    }

    /** A short pressure-varying squiggle, laid out on a grid across the page. */
    private fun scribble(
        index: Int,
        points: Int,
        random: Random,
    ): Stroke {
        val column = index % COLUMNS
        val row = index / COLUMNS
        val originX = column * CELL + random.nextDouble(-6.0, 6.0)
        val originY = row * CELL + random.nextDouble(-6.0, 6.0)
        val phase = random.nextDouble(0.0, 6.28)
        val samples =
            (0 until points).map { step ->
                val t = step.toDouble() / (points - 1)
                StrokePoint(
                    x = originX + t * CELL * 0.9,
                    y = originY + sin(phase + t * 9.0) * CELL * 0.3 + cos(t * 4.0) * 3.0,
                    t = (step * 6).toLong(),
                    pressure = 0.25 + 0.7 * (0.5 + 0.5 * sin(phase + t * 5.0)),
                )
            }
        return Stroke(
            points = samples,
            style =
                StrokeStyle(
                    styleVersion = SOLID_ROUND_PRESSURE_STYLE_VERSION,
                    parameters =
                        SolidRoundParameters(
                            color = COLORS[index % COLORS.size],
                            width = 2.0 + (index % 5),
                        ),
                ),
        )
    }

    private companion object {
        const val TAG = "DensePageSeeder"
        const val DEFAULT_STROKES = 1200
        const val DEFAULT_POINTS = 24
        const val COLUMNS = 24
        const val CELL = 90.0
        const val PAUSE_EVERY = 25
        const val PAUSE_MS = 120L
        const val SEED = 312L
        val COLORS = listOf("#000000", "#1565C0", "#006400", "#C62828", "#6A1B9A", "#EF6C00")
    }
}
