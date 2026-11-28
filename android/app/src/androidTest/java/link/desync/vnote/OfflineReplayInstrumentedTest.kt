package link.desync.vnote

import android.os.Build
import android.view.InputDevice
import android.view.MotionEvent
import android.view.View
import androidx.compose.ui.graphics.PixelMap
import androidx.compose.ui.semantics.SemanticsProperties
import androidx.compose.ui.semantics.getOrNull
import androidx.compose.ui.test.assertCountEquals
import androidx.compose.ui.test.assertTextEquals
import androidx.compose.ui.test.getUnclippedBoundsInRoot
import androidx.compose.ui.test.junit4.v2.createAndroidComposeRule
import androidx.compose.ui.test.onAllNodesWithTag
import androidx.compose.ui.test.onAllNodesWithText
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import link.desync.vnote.api.ApiClient
import link.desync.vnote.api.OkHttpApiClient
import link.desync.vnote.api.hostOf
import link.desync.vnote.auth.AuthConfig
import link.desync.vnote.auth.AuthRepository
import link.desync.vnote.auth.TokenStore
import link.desync.vnote.ink.DrawingToolPreferences
import link.desync.vnote.store.LocalStore
import link.desync.vnote.store.OpState
import mockwebserver3.Dispatcher
import mockwebserver3.MockResponse
import mockwebserver3.MockWebServer
import mockwebserver3.RecordedRequest
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.ExternalResource
import org.junit.rules.RuleChain
import org.junit.rules.TestRule
import org.junit.runner.RunWith
import java.net.InetAddress
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.atomic.AtomicInteger

/**
 * #159 end to end on the device: the app is local-first. It opens offline
 * from the stored identity, keeps every edit through a restart, and catches
 * up with the server on its own once it is back — each edit stored exactly
 * once, however often it was sent.
 *
 * #651: every failure is that same failure. Whatever goes wrong between the
 * app and the server — a proxy's 404 with the server down, a 403 off the VPN,
 * a captive portal's redirect or its page, a 500, a 401 a refresh does not
 * cure, an error code on an open page channel, no TLS, no answer, no DNS —
 * the page takes ink with no banner, the top bar shows the offline icon, and
 * everything owed replays with no user action once the server answers.
 * Nothing is refused, and nothing but having no token asks for a sign-in.
 *
 * "Offline" is the server shut down (and airplane mode on, API 30+), as in
 * [OfflineInstrumentedTest]; "back" is a new server on the same port with the
 * same canonical state, which lives in this class and outlives each server.
 * The other failures are a [Fault] the same server answers with, or another
 * address the app's client is pointed at ([SwitchableApiClient]).
 */
@RunWith(AndroidJUnit4::class)
class OfflineReplayInstrumentedTest {
    private lateinit var server: MockWebServer
    private lateinit var tokenStore: TokenStore
    private var apiClient: SwitchableApiClient? = null
    private val clients = CopyOnWriteArrayList<ApiClient>()
    private var serverClosed = false

    // What the server answers every request with, while it is not [Fault.None].
    @Volatile
    private var fault: Fault = Fault.None

    // Whether the identity provider renews the token when asked to.
    @Volatile
    private var refreshSucceeds = false

    // Server-side ends of the app's open channels, to drop when a fault starts.
    private val openSockets = CopyOnWriteArrayList<WebSocket>()
    private val pageSubscriptions = AtomicInteger()
    private val purgedUpgrades = AtomicInteger()
    private val context get() = InstrumentationRegistry.getInstrumentation().targetContext
    private val store get() = LocalStore.shared(context)

    // The server's canonical state: per page, its batches in seq order.
    private val batches = ConcurrentHashMap<String, CopyOnWriteArrayList<JSONObject>>()
    private val commitsReceived = ConcurrentHashMap<String, AtomicInteger>()
    private val created = CopyOnWriteArrayList<String>()
    private val purged = ConcurrentHashMap.newKeySet<String>()

    private val environmentRule =
        object : ExternalResource() {
            override fun before() {
                DrawingToolPreferences.clear(context)
                batches[PAGE_ID] = CopyOnWriteArrayList()
                startServer(java.net.ServerSocket(0).use { it.localPort })
                val serverUrl = server.url("/").toString().removeSuffix("/")
                tokenStore = TokenStore(context)
                tokenStore.clear()
                tokenStore.saveTokens(
                    accessToken = "access-token",
                    refreshToken = "refresh-token",
                    accessTokenExpiryEpochSeconds = System.currentTimeMillis() / 1000 + 3600,
                )
                MainActivity.apiClientFactory = { _, _ -> SwitchableApiClient(clientFor(serverUrl)).also { apiClient = it } }
            }

            override fun after() {
                setAirplaneMode(false)
                MainActivity.apiClientFactory = null
                clients.forEach { it.shutdown() }
                silentServer?.close()
                if (::tokenStore.isInitialized) tokenStore.clear()
                DrawingToolPreferences.clear(context)
                if (::server.isInitialized && !serverClosed) server.close()
            }
        }

    private val composeRule = createAndroidComposeRule<MainActivity>()

    @get:Rule
    val rules: TestRule = RuleChain.outerRule(SnapshotThreadGuard()).around(environmentRule).around(composeRule)

    /**
     * Acceptance 2, 11: with no network at all, a cold start opens straight
     * into the library from the stored identity — no error screen, no Retry.
     * Existing pages open and take ink, new pages can be made, and the status
     * icon on every screen says offline with what is waiting.
     */
    @Test
    fun aColdStartWithNoNetworkOpensTheLibraryAndEverythingStillWorks() {
        awaitTag("page-tile-$PAGE_ID")
        goOffline()
        composeRule.activityRule.scenario.recreate()

        awaitTag("page-tile-$PAGE_ID")
        assertEquals("no error screen", 0, composeRule.onAllNodesWithText("Retry").fetchSemanticsNodes().size)
        awaitTag("sync-status-offline")

        openPage(PAGE_ID)
        awaitTag("sync-status-offline")
        drawStroke()
        awaitOwed(1)
        leavePage()

        composeRule.onNodeWithTag("create-page-button").performClick()
        awaitTag("ink-canvas")
        drawStroke()
        leavePage()

        assertEquals("one page made here", 1, store.library.localOnlyIds().size)
        awaitOwed(3)
        composeRule.onNodeWithTag("sync-status-offline").performClick()
        awaitTag("sync-status-details")
        awaitTag("sync-status-pending")
        composeRule.onNodeWithTag("sync-status-host").assertExists()
        composeRule.onNodeWithTag("sync-status-error-class").assertExists()
        composeRule.onNodeWithTag("sync-status-last-sync").assertExists()
    }

    /**
     * Acceptance 1, 3, 15: a stroke drawn offline is on disk before it is
     * drawn, survives the app restarting, and is replayed on its own once the
     * server is back — stored exactly once, and not sent again after a
     * further restart.
     */
    @Test
    fun offlineInkSurvivesARestartAndIsStoredExactlyOnceWhenBack() {
        awaitTag("page-tile-$PAGE_ID")
        val port = server.port
        openPage(PAGE_ID)
        goOffline()
        drawStroke()
        awaitOwed(1)
        val batchId =
            store.outbox.read
                .forPage(PAGE_ID)
                .single()
                .opId
        assertEquals(
            "on disk, for the next process",
            1,
            LocalStore(context, "vnote-local.db").outbox.read.unsyncedCount(),
        )
        leavePage()

        composeRule.activityRule.scenario.recreate()
        openPage(PAGE_ID)
        val pixels = composeRule.awaitInkPixels { hasInkNear(it, STROKE_MID_X, STROKE_Y.toInt()) }
        assertTrue("the stroke is drawn after the restart", hasInkNear(pixels, STROKE_MID_X, STROKE_Y.toInt()))
        leavePage()

        comeBackOnline(port)
        awaitOwed(0, timeoutMillis = 45_000)
        assertEquals("stored once", listOf(batchId), batches.getValue(PAGE_ID).map { it.getString("client_batch_id") })

        val sent = commitsReceived.getValue(batchId).get()
        composeRule.activityRule.scenario.recreate()
        awaitTag("page-tile-$PAGE_ID")
        Thread.sleep(3_000)
        assertEquals("not sent again once acknowledged", sent, commitsReceived.getValue(batchId).get())
        assertEquals("still stored once", 1, batches.getValue(PAGE_ID).size)
    }

    /** A page made offline is created on the server under its own id, then its ink follows. */
    @Test
    fun aPageMadeOfflineIsCreatedUnderItsOwnIdWhenBack() {
        awaitTag("page-tile-$PAGE_ID")
        val port = server.port
        goOffline()
        composeRule.onNodeWithTag("create-page-button").performClick()
        awaitTag("ink-canvas")
        drawStroke()
        leavePage()
        val localId = store.library.localOnlyIds().single()

        comeBackOnline(port)
        awaitOwed(0, timeoutMillis = 45_000)

        assertEquals("created once, under the device's id", listOf(localId), created.toList())
        assertEquals("its stroke stored once", 1, batches.getValue(localId).size)
        assertTrue("no longer only on the device", store.library.localOnlyIds().isEmpty())
    }

    /**
     * #651, acceptance 10: edits owed to a page permanently deleted elsewhere
     * are answered 410 for good. They are not refused: they stay pending and
     * keep being retried, behind the same offline icon, with nothing on the
     * page. The icon's details offer — and only offer — to keep them as a new
     * page, which then syncs like any other. Nothing anywhere discards them.
     */
    @Test
    fun editsToAPageDeletedElsewhereStayOwedAndCanBeKeptAsANewPage() {
        awaitTag("page-tile-$PAGE_ID")
        val port = server.port
        openPage(PAGE_ID)
        goOffline()
        drawStroke()
        awaitOwed(1)
        leavePage()

        purged += PAGE_ID
        comeBackOnline(port)
        awaitTag("sync-gone-$PAGE_ID", timeoutMillis = 45_000, openDetails = true)
        composeRule.waitUntil(timeoutMillis = 45_000) { textOf("sync-status-error-class") == "Error: HTTP 410" }
        composeRule.onAllNodesWithText("Discard").assertCountEquals(0)
        composeRule.onAllNodesWithText("Sign in").assertCountEquals(0)
        composeRule.onNodeWithText("Close").performClick()
        assertEquals("nothing stored on the deleted page", 0, batches.getValue(PAGE_ID).size)
        assertTrue(
            "still owed, never refused",
            store.outbox.read
                .forPage(PAGE_ID)
                .map { it.state }
                .single() in setOf(OpState.Pending, OpState.Sent),
        )

        // Left alone, it goes on being retried.
        val seen = purgedUpgrades.get()
        composeRule.waitUntil(timeoutMillis = 60_000) { purgedUpgrades.get() >= seen + 2 }
        assertEquals("and is still owed", 1, store.outbox.read.unsyncedCount())
        assertNothingOnThePage()

        awaitTag("sync-gone-$PAGE_ID", timeoutMillis = 45_000, openDetails = true)
        composeRule.onNodeWithTag("sync-gone-keep").performClick()
        awaitTag("ink-canvas")

        awaitOwed(0, timeoutMillis = 45_000)
        val newPage = created.single()
        assertEquals("the stroke landed on the new page, once", 1, batches.getValue(newPage).size)
        assertEquals("and never on the deleted one", 0, batches.getValue(PAGE_ID).size)
        awaitNoIcon()
    }

    /**
     * #651, acceptance 1, 2, 4 — the report itself. With the v-note container
     * stopped, Traefik has no router for the host and answers every request
     * with its own plain-text 404. No banner appears, drawing and creating
     * pages work, the icon's details name the host, `HTTP 404` and the last
     * sync — through a restart too — and everything syncs by itself once the
     * container is back.
     */
    @Test
    fun aProxys404WithTheServerDownKeepsEverythingAndRecoversByItself() {
        awaitTag("page-tile-$PAGE_ID")
        openPage(PAGE_ID)
        awaitLive()

        breakWith(Fault.Status(404, "404 page not found\n"))
        drawStroke()
        awaitOwed(1)
        awaitTag("sync-status-offline", timeoutMillis = 30_000)
        assertNothingOnThePage()
        leavePage()
        composeRule.onNodeWithTag("create-page-button").performClick()
        awaitTag("ink-canvas")
        drawStroke()
        assertNothingOnThePage()
        leavePage()
        val localId = store.library.localOnlyIds().single()
        awaitOwed(3)

        composeRule.activityRule.scenario.recreate()
        awaitTag("page-tile-$PAGE_ID")
        awaitTag("page-tile-$localId")
        assertEquals("every edit is still owed after the restart", 3, store.outbox.read.unsyncedCount())
        awaitTag("sync-status-offline", timeoutMillis = 30_000)
        composeRule.onNodeWithTag("sync-status-offline").performClick()
        awaitTag("sync-status-details")
        composeRule.onNodeWithTag("sync-status-pending").assertTextEquals("3 changes are waiting to sync")
        composeRule.onNodeWithTag("sync-status-host").assertTextEquals("Can't reach ${hostOf(checkNotNull(apiClient).baseUrl)}")
        composeRule.onNodeWithTag("sync-status-error-class").assertTextEquals("Error: HTTP 404")
        composeRule.onNodeWithTag("sync-status-last-sync").assertExists()
        assertNoWayOutButWaiting()
        composeRule.onNodeWithText("Close").performClick()
        assertTrue("nothing reached the server", batches.values.all { it.isEmpty() } && created.isEmpty())

        fault = Fault.None
        awaitOwed(0, timeoutMillis = 60_000)
        assertEquals("the existing page's stroke, once", 1, batches.getValue(PAGE_ID).size)
        assertEquals("the page made meanwhile, once, under the device's id", listOf(localId), created.toList())
        assertEquals("and its stroke, once", 1, batches.getValue(localId).size)
        awaitNoIcon()
        assertNeverSignedOut()
    }

    /** A 403 from a proxy: what `lan-vpn-only` answers a phone off the LAN and the VPN. */
    @Test
    fun aProxys403StaysOfflineAndRecovers() = staysOfflineAndRecovers(setOf("HTTP 403"), { breakWith(Fault.Status(403, "Forbidden")) })

    /** A captive portal's redirect. It is not followed: it is a failed request. */
    @Test
    fun aRedirectStaysOfflineAndRecovers() =
        staysOfflineAndRecovers(
            setOf("HTTP 302"),
            { breakWith(Fault.Status(302, "", location = "http://127.0.0.1:9/portal/login")) },
        )

    /** Any other 4xx — statuses that used to end a page or a create for good among them. */
    @Test
    fun anyOtherClientErrorStaysOfflineAndRecovers() {
        staysOfflineAndRecovers(setOf("HTTP 400"), { breakWith(Fault.Status(400, "Bad Request")) })
    }

    @Test
    fun aConflictStaysOfflineAndRecovers() = staysOfflineAndRecovers(setOf("HTTP 409"), { breakWith(Fault.Status(409, "Conflict")) })

    /** A server bug: retried like a network failure, and back by itself once it is fixed. */
    @Test
    fun aServerErrorStaysOfflineAndRecoversThroughARestart() =
        staysOfflineAndRecovers(setOf("HTTP 500"), { breakWith(Fault.Status(500, "Internal Server Error")) }, restart = true)

    /**
     * A captive portal's own page, served with 200 for every URL: a body that
     * is not v-note's is a failure like any other, never "sign in" — and a
     * restart into it (whose `/me` gets the page too) changes nothing.
     */
    @Test
    fun aPageThatIsNotVNotesStaysOfflineAndRecoversThroughARestart() =
        staysOfflineAndRecovers(setOf("HTTP 200", "JSONException"), { breakWith(Fault.HtmlOk) }, restart = true)

    /**
     * A 401 that is still a 401 after the identity provider renewed the
     * token: not an expired session. Nothing asks for a sign-in, and no token
     * is cleared.
     */
    @Test
    fun a401ThatOutlastsARefreshStaysOfflineAndRecovers() {
        refreshSucceeds = true
        staysOfflineAndRecovers(setOf("HTTP 401"), { breakWith(Fault.Status(401, "Unauthorized")) })
    }

    /** An error code on an open page channel — one that used to refuse the page for good. */
    @Test
    fun anErrorCodeOnAnOpenPageChannelStaysOfflineAndRecovers() =
        staysOfflineAndRecovers(setOf("forbidden"), { fault = Fault.ChannelError("forbidden") })

    /** Connection refused: nothing listens where the server was. */
    @Test
    fun aRefusedConnectionStaysOfflineAndRecovers() {
        var port = 0
        staysOfflineAndRecovers(
            setOf("offline"),
            {
                port = server.port
                server.close()
                serverClosed = true
            },
            fixIt = { startServer(port) },
        )
    }

    /** A TLS failure: the address answers, but not with TLS. */
    @Test
    fun aTlsFailureStaysOfflineAndRecovers() {
        val good = checkNotNull(apiClient).current
        staysOfflineAndRecovers(
            setOf("offline"),
            { breakBySwitchingTo(clientFor("https://${server.url("/").host}:${server.port}")) },
            fixIt = { comeBackTo(good) },
        )
    }

    /** A timeout: the address accepts the connection and never answers. */
    @Test
    fun aTimeoutStaysOfflineAndRecovers() {
        val good = checkNotNull(apiClient).current
        val silent = SilentServer().also { silentServer = it }
        staysOfflineAndRecovers(
            setOf("timeout", "offline"),
            { breakBySwitchingTo(clientFor("http://${server.url("/").host}:${silent.port}")) },
            fixIt = { comeBackTo(good) },
        )
    }

    /** A DNS failure: the server's name does not resolve. */
    @Test
    fun aDnsFailureStaysOfflineAndRecovers() =
        staysOfflineAndRecovers(
            setOf("offline"),
            {
                fault = Fault.Away
                checkNotNull(apiClient).dnsDown = true
                dropOpenSockets()
            },
            fixIt = {
                fault = Fault.None
                checkNotNull(apiClient).dnsDown = false
            },
        )

    /**
     * #651, acceptance 6: the one thing the app cannot fix by itself is
     * having no token — here, the identity provider refuses the refresh. The
     * same icon shows, and only now do its details offer Sign in. Editing
     * carries on and everything stays owed.
     */
    @Test
    fun onlyARefusedRefreshAsksForASignIn() {
        awaitTag("page-tile-$PAGE_ID")
        openPage(PAGE_ID)
        awaitLive()

        refreshSucceeds = false
        breakWith(Fault.Status(401, "Unauthorized"))
        drawStroke()
        awaitOwed(1)

        awaitTag("sync-status-sign-in", timeoutMillis = 30_000)
        assertNothingOnThePage()
        composeRule.onNodeWithTag("sync-status-sign-in").performClick()
        awaitTag("sync-status-details")
        // The dialog's own button; the icon behind it shares the tag.
        composeRule.onNodeWithText("Sign in").assertExists()
        assertEquals("the stroke is still owed", 1, store.outbox.read.unsyncedCount())
    }

    /**
     * One failure kind, start to finish (#651, acceptance 3): the page is
     * open and live, the failure starts, and a stroke is drawn into it. The
     * stroke is kept and nothing reaches the server; the offline icon shows,
     * naming the failure; the page shows no banner and nothing asks for a
     * sign-in or offers to discard. With [restart], the app restarts into
     * the failure and nothing changes. Once [fixIt] ends the failure, the
     * stroke is stored — exactly once — and the icon goes, with no user action.
     */
    private fun staysOfflineAndRecovers(
        expectedErrors: Set<String>,
        breakIt: () -> Unit,
        restart: Boolean = false,
        fixIt: () -> Unit = { fault = Fault.None },
    ) {
        awaitTag("page-tile-$PAGE_ID")
        openPage(PAGE_ID)
        awaitLive()

        breakIt()
        drawStroke()
        awaitOwed(1)
        awaitTag("sync-status-offline", timeoutMillis = 30_000)
        assertNothingOnThePage()

        if (restart) {
            leavePage()
            composeRule.activityRule.scenario.recreate()
            awaitTag("page-tile-$PAGE_ID")
            assertEquals("still owed after the restart", 1, store.outbox.read.unsyncedCount())
            awaitTag("sync-status-offline", timeoutMillis = 30_000)
        }
        composeRule.onNodeWithTag("sync-status-offline").performClick()
        awaitTag("sync-status-details")
        composeRule.onNodeWithTag("sync-status-pending").assertTextEquals("1 change is waiting to sync")
        composeRule.onNodeWithTag("sync-status-host").assertTextEquals("Can't reach ${hostOf(checkNotNull(apiClient).baseUrl)}")
        composeRule.waitUntil(timeoutMillis = 30_000) { textOf("sync-status-error-class").removePrefix("Error: ") in expectedErrors }
        assertNoWayOutButWaiting()
        composeRule.onNodeWithText("Close").performClick()
        assertEquals("nothing reached the server", 0, batches.getValue(PAGE_ID).size)

        fixIt()
        awaitOwed(0, timeoutMillis = 60_000)
        assertEquals("stored exactly once", 1, batches.getValue(PAGE_ID).size)
        awaitNoIcon()
        assertNeverSignedOut()
    }

    // The fault starts: every request is answered with it from now on, and
    // the channels already open are dropped, as they are when a server goes.
    private fun breakWith(next: Fault) {
        fault = next
        dropOpenSockets()
    }

    // The app's client now reaches [broken] instead of the server, and the
    // channels it had open to the server are dropped.
    private fun breakBySwitchingTo(broken: ApiClient) {
        fault = Fault.Away
        checkNotNull(apiClient).current = broken
        dropOpenSockets()
    }

    private fun comeBackTo(good: ApiClient) {
        fault = Fault.None
        checkNotNull(apiClient).current = good
    }

    private fun dropOpenSockets() {
        openSockets.forEach { it.close(1012, "service restart") }
        openSockets.clear()
    }

    // A real client for [baseUrl], with an identity provider that renews the
    // token, or refuses to, as [refreshSucceeds] says — it cannot be reached
    // from a loopback-only test.
    private fun clientFor(baseUrl: String): ApiClient {
        val provider =
            object : AuthRepository(context, AuthConfig.fromBuildConfig(), tokenStore) {
                override suspend fun refreshAccessTokenIfNeeded(force: Boolean): Boolean = refreshSucceeds
            }
        return OkHttpApiClient(baseUrl, tokenStore, provider).also { clients += it }
    }

    // The open page's channel has caught up: what it sends now would land.
    private fun awaitLive() {
        composeRule.waitUntil(timeoutMillis = 10_000) { pageSubscriptions.get() >= 1 }
        composeRule.waitForIdle()
    }

    // #651: sync trouble never puts anything on the page.
    private fun assertNothingOnThePage() {
        for (kind in listOf("connecting", "disconnected", "retrying", "session-expired", "rejected")) {
            composeRule.onAllNodesWithTag("page-status-$kind").assertCountEquals(0)
        }
        composeRule.onAllNodesWithText("Keep as a new page").assertCountEquals(0)
        composeRule.onAllNodesWithText("Discard").assertCountEquals(0)
    }

    // The details of an ordinary failure offer nothing to do: it recovers by
    // itself. No sign-in, nothing to discard, nothing to keep elsewhere.
    private fun assertNoWayOutButWaiting() {
        composeRule.onAllNodesWithText("Sign in").assertCountEquals(0)
        composeRule.onAllNodesWithText("Discard").assertCountEquals(0)
        composeRule.onAllNodesWithText("Keep as a new page").assertCountEquals(0)
    }

    private fun assertNeverSignedOut() {
        assertTrue("the tokens are kept", tokenStore.hasSession())
        composeRule.onAllNodesWithTag("sync-status-sign-in").assertCountEquals(0)
        composeRule.onAllNodesWithText("Sign in").assertCountEquals(0)
    }

    private fun awaitNoIcon() {
        composeRule.waitUntil(timeoutMillis = 60_000) {
            listOf("offline", "sign-in").all { state ->
                composeRule.onAllNodesWithTag("sync-status-$state").fetchSemanticsNodes().isEmpty()
            }
        }
    }

    private fun textOf(tag: String): String =
        composeRule
            .onAllNodesWithTag(tag)
            .fetchSemanticsNodes()
            .firstOrNull()
            ?.config
            ?.getOrNull(SemanticsProperties.Text)
            ?.joinToString("") { it.text }
            .orEmpty()

    private fun awaitOwed(
        count: Int,
        timeoutMillis: Long = 10_000,
    ) {
        val reached =
            runCatching {
                composeRule.waitUntil(timeoutMillis) { store.outbox.read.unsyncedCount() == count }
            }
        check(reached.isSuccess) {
            "expected $count owed, found ${store.outbox.read.unsyncedCount()}: " +
                store.outbox.read
                    .forPage(PAGE_ID)
                    .map { "${it.kind}/${it.state}" } +
                " created=$created fault=$fault"
        }
    }

    private fun openPage(pageId: String) {
        awaitTag("page-tile-$pageId")
        composeRule.onNodeWithTag("page-tile-$pageId").performClick()
        awaitTag("ink-canvas")
    }

    private fun leavePage() {
        composeRule.onNodeWithContentDescription("Back to library").performClick()
        awaitTag("create-page-button")
    }

    private fun drawStroke() {
        val downTime = android.os.SystemClock.uptimeMillis()
        sendStylus(MotionEvent.ACTION_DOWN, STROKE_START_X, downTime)
        for (x in (STROKE_START_X.toInt() + 20)..STROKE_END_X.toInt() step 20) {
            sendStylus(MotionEvent.ACTION_MOVE, x.toFloat(), downTime)
        }
        sendStylus(MotionEvent.ACTION_UP, STROKE_END_X, downTime)
        composeRule.waitForIdle()
    }

    private fun sendStylus(
        action: Int,
        localX: Float,
        downTime: Long,
    ) {
        val canvas = composeRule.onNodeWithTag("ink-canvas").getUnclippedBoundsInRoot()
        val density = composeRule.activity.resources.displayMetrics.density
        val contentLocation = IntArray(2)
        composeRule.activity
            .findViewById<View>(android.R.id.content)
            .getLocationOnScreen(contentLocation)
        val properties = arrayOf(MotionEvent.PointerProperties().apply { toolType = MotionEvent.TOOL_TYPE_STYLUS })
        val coordinates =
            arrayOf(
                MotionEvent.PointerCoords().apply {
                    x = contentLocation[0] + canvas.left.value * density + localX
                    y = contentLocation[1] + canvas.top.value * density + STROKE_Y
                    pressure = 1f
                    size = 0.1f
                },
            )
        val event =
            MotionEvent.obtain(
                downTime,
                android.os.SystemClock.uptimeMillis(),
                action,
                1,
                properties,
                coordinates,
                0,
                0,
                1f,
                1f,
                0,
                0,
                InputDevice.SOURCE_STYLUS,
                0,
            )
        val decor = composeRule.activity.window.decorView
        val decorLocation = IntArray(2)
        decor.getLocationOnScreen(decorLocation)
        event.offsetLocation(-decorLocation[0].toFloat(), -decorLocation[1].toFloat())
        composeRule.runOnUiThread { decor.dispatchTouchEvent(event) }
        event.recycle()
    }

    // The default pen is dark green; anything green-dominant near the point is ink.
    private fun hasInkNear(
        pixels: PixelMap,
        x: Int,
        y: Int,
    ): Boolean =
        (-INK_SEARCH..INK_SEARCH).any { dx ->
            (-INK_SEARCH..INK_SEARCH).any { dy ->
                val px = x + dx
                val py = y + dy
                px in 0 until pixels.width &&
                    py in 0 until pixels.height &&
                    pixels[px, py].let { it.green > it.red + 0.15f && it.green > it.blue + 0.15f }
            }
        }

    // [openDetails]: the tag is inside the status icon's details, which are
    // opened as soon as the icon is there.
    private fun awaitTag(
        tag: String,
        timeoutMillis: Long = 10_000,
        openDetails: Boolean = false,
    ) {
        composeRule.waitUntil(timeoutMillis) {
            if (openDetails && composeRule.onAllNodesWithTag("sync-status-details").fetchSemanticsNodes().isEmpty()) {
                val icon = composeRule.onAllNodesWithTag("sync-status-offline")
                if (icon.fetchSemanticsNodes().isNotEmpty()) icon[0].performClick()
            }
            composeRule.onAllNodesWithTag(tag).fetchSemanticsNodes().isNotEmpty()
        }
    }

    private fun goOffline() {
        setAirplaneMode(true)
        server.close()
        serverClosed = true
    }

    private fun comeBackOnline(port: Int) {
        setAirplaneMode(false)
        startServer(port)
    }

    // An explicit port, so "back" is the same address; MockWebServer sets
    // SO_REUSEADDR only for one (see [OfflineInstrumentedTest]).
    private fun startServer(port: Int) {
        server = MockWebServer()
        server.dispatcher = dispatcher()
        server.start(InetAddress.getByName("127.0.0.1"), port)
        serverClosed = false
    }

    private fun setAirplaneMode(enabled: Boolean) {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.R) return
        val state = if (enabled) "enable" else "disable"
        InstrumentationRegistry
            .getInstrumentation()
            .uiAutomation
            .executeShellCommand("cmd connectivity airplane-mode $state")
            .close()
    }

    private fun dispatcher(): Dispatcher =
        object : Dispatcher() {
            override fun dispatch(request: RecordedRequest): MockResponse {
                val path = request.url.encodedPath
                when (val failing = fault) {
                    is Fault.Status -> return failing.response()
                    Fault.HtmlOk -> return portalPage()
                    else -> Unit
                }
                return when {
                    path == "/api/me" -> json("""{"sub":"replay-user","email":"test@example.com"}""")
                    path == "/api/pages" && request.method == "POST" -> create(JSONObject(request.body?.utf8().orEmpty()))
                    // A permanently deleted page is not in the library any more.
                    path == "/api/pages" ->
                        json(JSONObject().put("pages", JSONArray().apply { if (PAGE_ID !in purged) put(pageJson(PAGE_ID)) }).toString())
                    path == "/api/realtime" -> MockResponse.Builder().webSocketUpgrade(libraryChannel()).build()
                    path.startsWith("/api/pages/") && path.endsWith("/realtime") -> {
                        val pageId = path.removePrefix("/api/pages/").removeSuffix("/realtime")
                        if (pageId in purged) {
                            purgedUpgrades.incrementAndGet()
                            MockResponse(code = 410)
                        } else {
                            MockResponse.Builder().webSocketUpgrade(pageChannel(pageId)).build()
                        }
                    }
                    else -> MockResponse(code = 404)
                }
            }
        }

    private fun create(body: JSONObject): MockResponse {
        val id = body.getString("id")
        if (id !in created) created += id
        batches.putIfAbsent(id, CopyOnWriteArrayList())
        return MockResponse
            .Builder()
            .code(201)
            .body(JSONObject().put("page", pageJson(id)).toString())
            .addHeader("Content-Type", "application/json")
            .build()
    }

    // The server side of one page's channel: replays what it holds after the
    // client's cursor, and stores each batch once, echoing every commit.
    private fun pageChannel(pageId: String) =
        object : WebSocketListener() {
            private val held get() = batches.getOrPut(pageId) { CopyOnWriteArrayList() }

            override fun onOpen(
                webSocket: WebSocket,
                response: okhttp3.Response,
            ) {
                openSockets += webSocket
                webSocket.send(
                    JSONObject()
                        .put("type", "welcome")
                        .put("session_id", "s")
                        .put("last_seq", held.size)
                        .toString(),
                )
            }

            override fun onMessage(
                webSocket: WebSocket,
                text: String,
            ) {
                val message = JSONObject(text)
                val failing = fault
                when (message.getString("type")) {
                    "subscribe" -> {
                        val from = message.getLong("from_seq")
                        held.filter { it.getLong("seq") > from }.forEach { webSocket.send(it.toString()) }
                        webSocket.send(JSONObject().put("type", "synced").put("last_seq", held.size).toString())
                        pageSubscriptions.incrementAndGet()
                    }
                    "commit-batch" ->
                        when (failing) {
                            // A channel the fault is about to drop stores nothing.
                            is Fault.Status, Fault.HtmlOk, Fault.Away -> Unit
                            is Fault.ChannelError ->
                                webSocket.send(
                                    JSONObject()
                                        .put("type", "error")
                                        .put("code", failing.code)
                                        .put("message", "refused by the test server")
                                        .toString(),
                                )
                            Fault.None -> webSocket.send(store(message).toString())
                        }
                }
            }

            // Stores a batch once, however often it is committed; the echo
            // is the stored batch either way.
            private fun store(message: JSONObject): JSONObject {
                val id = message.getString("client_batch_id")
                commitsReceived.getOrPut(id) { AtomicInteger() }.incrementAndGet()
                return synchronized(held) {
                    held.firstOrNull { it.getString("client_batch_id") == id }
                        ?: JSONObject()
                            .put("type", "stroke-batch")
                            .put("seq", held.size + 1)
                            .put("client_batch_id", id)
                            .put("strokes", message.getJSONArray("strokes"))
                            .also { held += it }
                }
            }

            override fun onClosing(
                webSocket: WebSocket,
                code: Int,
                reason: String,
            ) {
                webSocket.close(code, reason)
            }
        }

    private fun pageJson(id: String): JSONObject =
        JSONObject()
            .put("id", id)
            .put("title", if (id == PAGE_ID) "Replay test" else "")
            .put("created_at", "2026-11-28T00:00:00Z")
            .put("updated_at", "2026-11-28T00:00:00Z")
            .put("paper", "none")

    private fun libraryChannel() =
        object : WebSocketListener() {
            override fun onOpen(
                webSocket: WebSocket,
                response: okhttp3.Response,
            ) {
                openSockets += webSocket
            }

            override fun onClosing(
                webSocket: WebSocket,
                code: Int,
                reason: String,
            ) {
                webSocket.close(code, reason)
            }
        }

    private fun json(body: String): MockResponse =
        MockResponse
            .Builder()
            .code(200)
            .body(body)
            .addHeader("Content-Type", "application/json")
            .build()

    // What a captive portal serves for every URL: its own page, with 200.
    private fun portalPage(): MockResponse =
        MockResponse
            .Builder()
            .code(200)
            .body("<!doctype html><html><body><h1>Sign in to Wi-Fi</h1></body></html>")
            .addHeader("Content-Type", "text/html")
            .build()

    private var silentServer: SilentServer? = null

    // What stands between the app and the server, answering in its place.
    private sealed interface Fault {
        data object None : Fault

        // Every request, REST and upgrade alike, gets this status.
        data class Status(
            val code: Int,
            val body: String,
            val location: String? = null,
        ) : Fault {
            fun response(): MockResponse =
                MockResponse
                    .Builder()
                    .code(code)
                    .body(body)
                    .addHeader("Content-Type", "text/plain")
                    .apply { location?.let { addHeader("Location", it) } }
                    .build()
        }

        // Every request gets a web page that is not v-note's, with 200.
        data object HtmlOk : Fault

        // The app cannot get to the server at all (its client is pointed
        // somewhere broken): a channel still closing stores nothing.
        data object Away : Fault

        // The server is reachable; every commit on a page channel is
        // answered with this error code.
        data class ChannelError(
            val code: String,
        ) : Fault
    }

    // Accepts every connection and never says a word: a timeout.
    private class SilentServer : AutoCloseable {
        private val listening = java.net.ServerSocket(0, 50, InetAddress.getByName("127.0.0.1"))
        private val accepted = CopyOnWriteArrayList<java.net.Socket>()
        val port: Int get() = listening.localPort

        init {
            Thread({
                runCatching { while (true) accepted += listening.accept() }
            }, "silent-server").apply { isDaemon = true }.start()
        }

        override fun close() {
            listening.close()
            accepted.forEach { runCatching { it.close() } }
        }
    }

    private companion object {
        const val PAGE_ID = "page_replay"
        const val STROKE_Y = 300f
        const val STROKE_START_X = 240f
        const val STROKE_END_X = 440f
        const val STROKE_MID_X = 340
        const val INK_SEARCH = 6
    }
}
