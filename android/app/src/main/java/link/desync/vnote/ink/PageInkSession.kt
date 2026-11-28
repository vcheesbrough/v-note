package link.desync.vnote.ink

import android.os.SystemClock
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import link.desync.vnote.api.ApiClient
import link.desync.vnote.api.ConnectionFailure
import link.desync.vnote.api.PageEventListener
import link.desync.vnote.api.PageSocket
import link.desync.vnote.api.hostOf
import link.desync.vnote.auth.RefreshOutcome
import link.desync.vnote.model.PageEvent
import link.desync.vnote.model.Stroke
import link.desync.vnote.store.Edit
import link.desync.vnote.store.OpKind
import link.desync.vnote.sync.Backoff
import link.desync.vnote.sync.ErrorClass
import link.desync.vnote.telemetry.AppLog
import link.desync.vnote.telemetry.Attribute
import link.desync.vnote.telemetry.OpenSpan
import link.desync.vnote.telemetry.SpanContext
import link.desync.vnote.telemetry.Telemetry
import java.util.UUID

// Drives one page's ink channel: opens the page from the device's own copy,
// connects the WSS, replays persisted strokes (gap-fill), sends this device's
// edits, and applies other devices' strokes, erasures and paper live as they
// arrive — any number of devices may edit one page at once (#630).
//
// Local-first (#159): every edit is written to the durable outbox
// ([PageLedger]) before it is drawn, so editing works the same connected or
// not, and nothing is lost to a dropped socket, a killed process or leaving
// the page. Whenever the channel is caught up, everything still owed is sent
// again in the order it was made — the server is idempotent on every id, so a
// repeat is only re-echoed — until the server's echo acknowledges it. The
// channel reconnects on its own after any close it did not ask for (#279).
//
// Every failure is the same failure (#651): a refused upgrade whatever its
// status, a dropped socket, any error code on the open channel. Everything
// owed is kept and the channel tries again on the [Backoff] schedule until it
// works; nothing is ever refused for good. The one thing retrying cannot fix
// is having no token, which waits for a sign-in.
class PageInkSession(
    private val apiClient: ApiClient,
    private val pageId: String,
    private val scope: CoroutineScope,
    initialPaper: Paper = Paper.None,
    // Invoked only when a paper change *this session asked for* is confirmed in
    // force by the server. Deliberately not called for `Welcome`-carried paper
    // (which reports the page's existing paper, not a choice the user just
    // made) nor for a sibling device's change — otherwise merely opening a page
    // would rewrite the caller's new-page default.
    private val onPaperConfirmed: (Paper) -> Unit = {},
    private val sync: SessionSync,
) {
    private val ledger = PageLedger(sync.store, pageId)

    // Backed by a private state property rather than `var paper … private set`
    // because that would compile to a private `setPaper(Paper)` and clash with
    // the public `setPaper` below on the same JVM signature.
    private var paperState by mutableStateOf(initialPaper)

    // The page's paper: the newest one this device asked for, else the one in
    // force on the server (`Welcome` and `PaperChanged`), else the listing's.
    val paper: Paper get() = paperState

    // Rendered strokes: confirmed ink, then this device's ink on its way.
    var strokes by mutableStateOf<List<Stroke>>(emptyList())
        private set

    internal val pendingBatchCount: Int
        get() = ledger.pendingBatches.size

    // Strokes the server has not yet acknowledged. Kept in the outbox, so
    // leaving the page loses none of them; the sync engine sends them on.
    val unsentStrokeCount: Int
        get() = ledger.unsentStrokeCount

    // True while this session's socket is live and caught up: welcomed, and
    // its `subscribe` replay applied. Edits are accepted either way (#159);
    // this only says whether they are going out now.
    var canEdit by mutableStateOf(false)
        private set

    // Non-null when there is something to say about this page's sync (#150):
    // connecting, disconnected, retrying, or no token to sync with. None of
    // it is shown on the page (#651): that is the top bar's status icon.
    var status by mutableStateOf<PageStatus?>(null)
        private set

    // The status in words, for callers that only need the copy.
    val statusBanner: String? get() = status?.message

    // When the page was moved to the bin (#631), or null while it is live. Ink
    // committed to a binned page is still stored — the page just stays in the
    // bin — so this is a notice, not a block.
    var binnedAt by mutableStateOf<String?>(null)
        private set

    // Why the notice's last Restore failed; null once a restore lands.
    var restoreError by mutableStateOf<String?>(null)
        private set

    private var socket: PageSocket? = null

    // True from `subscribe` until the server's `synced`: replayed batches are
    // applied as they arrive but painted once, at the end (#323).
    private var replaying = false
    private var replayBatches = 0
    private var replayStartedAt = 0L

    // Telemetry (#406). One trace per open page, rooted when it connects; a
    // headless sync session is one span in whatever trace is current instead,
    // so it never takes over the screen's trace.
    private var screen: SpanContext = Telemetry.screen()
    private var syncSpan: OpenSpan? = null
    private var connectSpan: OpenSpan? = null
    private var subscribeSpan: OpenSpan? = null
    private val strokeSpans = hashMapOf<String, StrokeSpans>()

    // Set by [disconnect]: the close that follows is ours, not a failure.
    @Volatile
    private var closing = false

    // Bumped for every socket this session opens and whenever one is retired, so
    // a late callback from a socket a reconnect replaced is ignored.
    @Volatile
    private var socketGeneration = 0

    private var reconnectJob: Job? = null
    private val reconnectBackoff = Backoff()

    // An error code on the open channel (#150, #651): everything owed is sent
    // again on the same schedule as a reconnect.
    private var resendJob: Job? = null
    private val resendBackoff = Backoff()

    // Set once a 401 on the upgrade has been answered with a refresh. A 401
    // with the fresh token is a connection failure like any other, so the
    // next attempt, after its backoff, starts with a refresh again.
    private var refreshedAfterUnauthorized = false

    fun connect() {
        if (sync.headless) {
            syncSpan = Telemetry.span("sync.page").attr("vnote.page_id", pageId)
            screen = syncSpan?.context ?: screen
        } else {
            screen = Telemetry.startScreen("screen.page", listOf(Attribute("vnote.page_id", pageId)))
        }
        ledger.load()
        paperState = ledger.requestedPaper ?: ledger.confirmedPaper ?: paperState
        publishRenderableStrokes()
        if (sync.createPending()) {
            // Made on this device and not yet on the server: drawing works,
            // and the channel opens once the sync engine's create lands.
            status = PageStatus(PageStatusKind.Disconnected, WAITING_FOR_CREATE)
            return
        }
        status = PageStatus(PageStatusKind.Connecting, CONNECTING)
        open()
    }

    // The sync engine's create for this page landed: open its channel.
    fun createAcknowledged() {
        if (closing || socket != null) return
        status = PageStatus(PageStatusKind.Connecting, CONNECTING)
        open()
    }

    // Reconnect now instead of waiting out the backoff — the network may be
    // back — and start over once the user has signed in again.
    fun retryNow() {
        if (closing || status?.kind !in RETRYABLE) {
            return
        }
        AppLog.i(TAG, "page channel retry requested", context = screen)
        reconnectJob?.cancel()
        reconnectJob = null
        stopResend()
        refreshedAfterUnauthorized = false
        reconnectBackoff.reset()
        retireSocket()
        open()
    }

    // Opens a page socket. Everything the session holds is kept, so the
    // `subscribe(lastSeq)` asks only for what it has not seen.
    private fun open() {
        val generation = ++socketGeneration
        connectSpan = Telemetry.span("realtime.connect", screen).attr("vnote.channel", "page")
        val opened =
            apiClient.openPageSocket(
                pageId,
                object : PageEventListener {
                    // Pinned to the main thread: these callbacks arrive on
                    // OkHttp's reader thread and every handler writes Compose
                    // state, which must not race the UI's snapshot reads (#465).
                    override fun onEvent(event: PageEvent) {
                        scope.launch(Dispatchers.Main) { if (generation == socketGeneration) handle(event) }
                    }

                    override fun onError(message: String) {
                        onConnectionFailure(ConnectionFailure(message, hostOf(apiClient.baseUrl), ErrorClass.OFFLINE, null))
                    }

                    override fun onConnectionFailure(failure: ConnectionFailure) {
                        scope.launch(Dispatchers.Main) { if (generation == socketGeneration) handleDisconnected(failure) }
                    }

                    override fun onUnauthorized(message: String) {
                        scope.launch(Dispatchers.Main) { if (generation == socketGeneration) handleUnauthorized(message) }
                    }

                    override fun onClosed() {
                        scope.launch(Dispatchers.Main) {
                            if (generation == socketGeneration) {
                                handleDisconnected(ConnectionFailure(CLOSED_DETAIL, hostOf(apiClient.baseUrl), ErrorClass.OFFLINE, null))
                            }
                        }
                    }
                },
            )
        socket = opened
        if (opened == null) {
            connectSpan?.fail(NOT_SIGNED_IN)
            connectSpan = null
            sessionExpired(NOT_SIGNED_IN, detail = null)
        }
    }

    fun disconnect() {
        closing = true
        if (ledger.hasWork) {
            AppLog.i(TAG, "leaving the page with edits still to sync: strokes=$unsentStrokeCount", context = screen)
        }
        reconnectJob?.cancel()
        reconnectJob = null
        stopResend()
        failStrokeSpans("page closed")
        socket?.close()
        socket = null
        syncSpan?.end()
        syncSpan = null
    }

    // A freshly captured stroke. Written to the outbox before it is drawn —
    // accepted locally means it survives a crash — and sent now if the channel
    // is caught up, or by [resendPending] once it is.
    fun commitStroke(stroke: Stroke) {
        if (stroke.points.isEmpty()) {
            return
        }
        val clientBatchId = "batch_${newId()}"
        ledger.record(Edit.Batch(clientBatchId, listOf(stroke)))
        strokeSpans[clientBatchId] = StrokeSpans.open(screen, clientBatchId, stroke)
        publishRenderableStrokes()
        sendIfLive(clientBatchId) { it.commitBatch(clientBatchId, listOf(stroke)) }
    }

    // Change the page's paper. Optimistic and durable: drawn at once, kept in
    // the outbox until the server's `PaperChanged` confirms it. Paper is last
    // arrival wins, so another device's change landing later simply wins.
    //
    // Durable side effects are deferred to [onPaperConfirmed]: a caller must
    // not persist a choice the server never put in force.
    fun setPaper(next: Paper) {
        if (next == paper && ledger.requestedPaper == null) {
            // Already in force server-side: nothing to send or wait for.
            onPaperConfirmed(next)
            return
        }
        val clientMutationId = "paper_${newId()}"
        ledger.record(Edit.SetPaper(clientMutationId, next))
        paperState = next
        sendIfLive(clientMutationId) { it.setPaper(clientMutationId, next) }
    }

    fun eraseStrokes(strokeIds: Collection<String>) {
        if (strokeIds.isEmpty()) return
        val clientMutationId = "erase_${newId()}"
        val uniqueIds = strokeIds.toSet()
        ledger.record(Edit.Erase(clientMutationId, uniqueIds))
        publishRenderableStrokes()
        sendIfLive(clientMutationId) { it.commitTombstones(clientMutationId, uniqueIds.toList()) }
    }

    private fun sendIfLive(
        opId: String,
        send: (PageSocket) -> Unit,
    ) {
        val activeSocket = socket
        if (canEdit && activeSocket != null) {
            send(activeSocket)
            sync.store.outbox.markSent(opId)
        } else {
            AppLog.i(TAG, "edit kept for sync: owed=${ledger.replayable().size}", context = screen)
            refreshSyncCounts()
        }
    }

    private fun handle(event: PageEvent) {
        when (event) {
            is PageEvent.Welcome -> onWelcome(event)
            is PageEvent.StrokeBatch -> onStrokeBatch(event)
            is PageEvent.PageReplay -> onPageReplay(event)
            is PageEvent.Synced -> onSynced(event.lastSeq)
            is PageEvent.TombstoneBatch -> onTombstoneBatch(event)
            is PageEvent.PaperChanged -> onPaperChanged(event)
            is PageEvent.PageBinned -> binnedAt = event.binnedAt
            PageEvent.PageRestored -> restored()
            is PageEvent.Failure -> onFailure(event)
        }
    }

    // The notice's Restore: bring the page back from the bin.
    fun restore() {
        scope.launch(Dispatchers.Main) {
            apiClient.restorePage(pageId).fold(
                onSuccess = { restored() },
                onFailure = { failure ->
                    AppLog.w(TAG, "restoring the page from the bin failed", failure, context = screen)
                    restoreError = failure.message ?: RESTORE_FAILED
                },
            )
        }
    }

    private fun restored() {
        binnedAt = null
        restoreError = null
    }

    private fun onWelcome(event: PageEvent.Welcome) {
        connectSpan?.end()
        connectSpan = null
        AppLog.i(TAG, "page channel connected", context = screen)
        sync.status?.reportReachable()
        sync.status?.reportPresent(pageId)
        status = null
        // A binned page says so straight after `welcome`.
        restored()
        refreshedAfterUnauthorized = false
        // The paper in force; a paper this device still owes stays drawn
        // until the server has it.
        ledger.confirmPaper(event.paper)
        paperState = ledger.requestedPaper ?: event.paper
        beginReplay()
        socket?.subscribe(ledger.lastSeq)
    }

    // Every committed batch, whoever drew it. Applying it changes [strokes] —
    // the committed layer — never the stroke under the nib. Stacking order is
    // server arrival order.
    private fun onStrokeBatch(event: PageEvent.StrokeBatch) {
        val mine = event.clientBatchId in ledger.pendingBatches
        if (ledger.confirmBatch(event)) {
            if (replaying) replayBatches += 1
            publishUnlessReplaying()
        }
        if (mine) {
            strokeSpans.remove(event.clientBatchId)?.confirmed(event.seq)
            acknowledged()
        }
    }

    // The whole replay in one frame, applied in one store transaction and
    // painted once by the `synced` it carries.
    private fun onPageReplay(event: PageEvent.PageReplay) {
        sync.store.transaction {
            event.batches.forEach(::onStrokeBatch)
            event.tombstones.forEach(::onTombstoneBatch)
            ledger.advance(event.lastSeq)
        }
        onSynced(event.lastSeq)
    }

    private fun onSynced(lastSeq: Long) {
        ledger.advance(lastSeq)
        endReplay()
        onCaughtUp()
    }

    private fun onTombstoneBatch(event: PageEvent.TombstoneBatch) {
        val mine = event.clientMutationId in ledger.pendingErasures
        val settled = ledger.confirmTombstones(event)
        // A batch erased before its echo will never be confirmed: close its
        // spans now, as what happened, rather than as "page closed" later.
        settled.forEach { strokeSpans.remove(it)?.failed("erased") }
        publishUnlessReplaying()
        if (mine || settled.isNotEmpty()) acknowledged()
    }

    private fun onPaperChanged(event: PageEvent.PaperChanged) {
        val answered = ledger.confirmPaper(event.paper)
        paperState = ledger.requestedPaper ?: event.paper
        if (answered) {
            onPaperConfirmed(event.paper)
            acknowledged()
        }
    }

    // The socket is welcomed and its replay applied: send everything owed.
    private fun onCaughtUp() {
        if (socket == null) {
            return
        }
        canEdit = true
        // Reset here, not on `welcome`: a socket that is welcomed and then
        // fails its replay every time must still back off.
        reconnectBackoff.reset()
        resendPending()
        idleIfDone()
    }

    // Everything still owed, in the order it was made. The replay has been
    // applied, so what the server already stored has been acknowledged and is
    // not here; a repeat of what it stored meanwhile is only re-echoed.
    private fun resendPending() {
        val activeSocket = socket ?: return
        val owed = ledger.replayable()
        if (owed.isEmpty()) return
        AppLog.i(TAG, "sending edits owed to the server: entries=${owed.size}", context = screen)
        owed.forEach { op ->
            when (op.kind) {
                OpKind.CommitBatch -> activeSocket.commitBatch(op.opId, op.strokes)
                OpKind.Erase -> activeSocket.commitTombstones(op.opId, op.strokeIds)
                OpKind.SetPaper -> activeSocket.setPaper(op.opId, op.paper)
                OpKind.CreatePage -> Unit
            }
            sync.store.outbox.markSent(op.opId)
        }
    }

    // An edit of ours reached the server — which is the server answering, so
    // whatever was last reported as failing is not failing now (#651): an
    // error code on a commit is cleared by the commit that lands.
    private fun acknowledged() {
        sync.status?.reportReachable()
        sync.status?.markSynced()
        refreshSyncCounts()
        // Keyed on the status, not the job: the job clears itself before the
        // resend it makes is answered.
        if (status?.kind == PageStatusKind.Retrying && !ledger.hasWork) {
            AppLog.i(TAG, "failed edits recovered", context = screen)
            stopResend()
            status = null
        }
        idleIfDone()
    }

    private fun idleIfDone() {
        if (sync.headless && canEdit && !ledger.hasWork) sync.onIdle()
    }

    // An error code on the open channel. Whatever it says, nothing is
    // discarded and nothing is refused (#651): the edit was not stored (the
    // server says so instead of echoing), and everything owed is sent again
    // on the backoff until the server takes it.
    private fun onFailure(event: PageEvent.Failure) {
        // The code only: the message is the server's.
        AppLog.w(TAG, "page channel error", context = screen, attributes = listOf(Attribute("vnote.error_code", event.code)))
        // A replay that ends in an error still shows what arrived.
        endReplay(failure = event.code)
        sync.status?.reportFailure(hostOf(apiClient.baseUrl), event.code, null)
        if (ErrorClass.isGone(event.code)) sync.status?.reportGone(pageId)
        if (sync.headless) {
            standDown()
            return
        }
        scheduleResend(event)
    }

    private fun scheduleResend(event: PageEvent.Failure) {
        status = PageStatus(PageStatusKind.Retrying, SYNC_RETRYING, detail = "${event.code}: ${event.message}")
        if (resendJob?.isActive == true) return
        val delayMs = resendBackoff.next()
        AppLog.i(TAG, "retrying failed edits: delayMs=$delayMs", context = screen)
        resendJob =
            scope.launch {
                delay(delayMs)
                resendJob = null
                when {
                    closing -> Unit
                    canEdit -> resendPending()
                    // Failed before the replay caught up (`replay_failed`,
                    // `welcome_failed`): the socket stays open but will never
                    // reach `synced`, so start over on a new one, which
                    // re-subscribes from the cursor.
                    else -> handleDisconnected(ConnectionFailure(event.code, hostOf(apiClient.baseUrl), event.code, null))
                }
            }
    }

    private fun stopResend() {
        resendJob?.cancel()
        resendJob = null
        resendBackoff.reset()
    }

    // A headless session that failed does not retry by itself: it hands its
    // slot back, and the sync engine tries the page again on its backoff.
    private fun standDown() {
        canEdit = false
        retireSocket()
        sync.onIdle()
    }

    private fun beginReplay() {
        replaying = true
        replayBatches = 0
        replayStartedAt = SystemClock.elapsedRealtime()
        subscribeSpan = Telemetry.span("realtime.subscribe", screen).attr("vnote.channel", "page")
    }

    // Close the replay window and paint what arrived. Idempotent, because the
    // window can be closed by `synced`, by an error, or by the socket dropping.
    private fun endReplay(failure: String? = null) {
        if (!replaying) {
            return
        }
        replaying = false
        val span = subscribeSpan?.attr("vnote.replay.batches", replayBatches)
        subscribeSpan = null
        AppLog.d(
            TAG,
            "page replay applied: batches=$replayBatches durationMs=${SystemClock.elapsedRealtime() - replayStartedAt}",
            context = span?.context ?: screen,
        )
        if (failure == null) span?.end() else span?.fail(failure)
        publishRenderableStrokes()
    }

    private fun publishUnlessReplaying() {
        if (!replaying) {
            publishRenderableStrokes()
        }
    }

    private fun publishRenderableStrokes() {
        strokes = ledger.renderable()
    }

    private fun refreshSyncCounts() {
        sync.status?.refresh()
    }

    private fun handleDisconnected(failure: ConnectionFailure) {
        if (!closing) {
            AppLog.w(TAG, "page channel disconnected: ${failure.message}", context = screen)
        }
        connectSpan?.fail(failure.message)
        connectSpan = null
        // A socket that drops mid-replay never sends `synced`: paint what came.
        endReplay(failure = "disconnected")
        canEdit = false
        stopResend()
        if (closing) return
        sync.status?.reportFailure(failure.host, failure.errorClass, failure.correlationId)
        if (ErrorClass.isGone(failure.errorClass)) sync.status?.reportGone(pageId)
        status = PageStatus(PageStatusKind.Disconnected, DISCONNECTED, detail = failure.message)
        if (sync.headless) {
            standDown()
            return
        }
        scheduleReconnect()
    }

    // The upgrade was refused with 401. One forced refresh, then straight back
    // in. A 401 that is still a 401 with the fresh token is not the session's
    // fault — something in between is answering — and neither is an identity
    // provider out of reach: both are connection failures, retried (#651).
    // Only a refused refresh leaves no token, and waits for a sign-in.
    private fun handleUnauthorized(message: String) {
        AppLog.w(TAG, "page channel refused the session: $message", context = screen)
        connectSpan?.fail(message)
        connectSpan = null
        endReplay(failure = "unauthorized")
        canEdit = false
        stopResend()
        retireSocket()
        if (closing) return
        if (refreshedAfterUnauthorized) {
            handleDisconnected(ConnectionFailure(message, hostOf(apiClient.baseUrl), ErrorClass.http(HTTP_UNAUTHORIZED), null))
            return
        }
        refreshedAfterUnauthorized = true
        reconnectJob =
            scope.launch {
                val outcome = if (apiClient.refreshSession()) RefreshOutcome.Refreshed else apiClient.lastRefreshOutcome
                if (closing) return@launch
                when (outcome) {
                    RefreshOutcome.Refreshed -> {
                        AppLog.i(TAG, "token refreshed; reconnecting the page channel", context = screen)
                        open()
                    }
                    RefreshOutcome.Unavailable -> {
                        // This coroutine is the reconnect job: cleared so
                        // the reconnect it asks for can be scheduled.
                        reconnectJob = null
                        handleDisconnected(ConnectionFailure(message, apiClient.signInHost, ErrorClass.OFFLINE, null))
                    }
                    RefreshOutcome.Refused -> sessionExpired(SESSION_EXPIRED, detail = message)
                }
            }
    }

    // There is no token to connect with: only signing in again fixes this.
    // Editing carries on, and everything owed stays in the outbox for then.
    private fun sessionExpired(
        message: String,
        detail: String?,
    ) {
        AppLog.w(TAG, "page channel paused: no token to sync with", context = screen)
        sync.status?.requireSignIn()
        status = PageStatus(PageStatusKind.SessionExpired, message, detail = detail)
        sync.onIdle()
    }

    private fun retireSocket() {
        socketGeneration++
        socket?.close()
        socket = null
    }

    private fun scheduleReconnect() {
        if (reconnectJob?.isActive == true) {
            return
        }
        retireSocket()
        val delayMs = reconnectBackoff.next()
        reconnectJob =
            scope.launch {
                delay(delayMs)
                if (!closing) {
                    AppLog.i(TAG, "page channel reconnecting", context = screen)
                    refreshedAfterUnauthorized = false
                    open()
                }
            }
    }

    private fun failStrokeSpans(reason: String) {
        strokeSpans.values.forEach { it.failed(reason) }
        strokeSpans.clear()
    }

    companion object {
        private fun newId(): String = UUID.randomUUID().toString().replace("-", "")

        private const val HTTP_UNAUTHORIZED = 401
        private const val TAG = "PageInkSession"
        private const val RESTORE_FAILED = "Restoring the page failed"
        private const val CLOSED_DETAIL = "connection closed"
        private val RETRYABLE = setOf(PageStatusKind.Disconnected, PageStatusKind.SessionExpired)

        const val CONNECTING = "Connecting…"
        const val DISCONNECTED = "Offline — your changes are kept and sync when you're back"
        const val WAITING_FOR_CREATE = "New page — it syncs once the server is reachable"
        const val SYNC_RETRYING = "Couldn't save your ink — retrying"
        const val SESSION_EXPIRED = "Sign in to sync"
        const val NOT_SIGNED_IN = "Not signed in"
    }
}
