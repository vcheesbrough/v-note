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
import link.desync.vnote.api.PageEventListener
import link.desync.vnote.api.PageSocket
import link.desync.vnote.model.PageEvent
import link.desync.vnote.model.Stroke
import link.desync.vnote.telemetry.AppLog
import link.desync.vnote.telemetry.Attribute
import link.desync.vnote.telemetry.OpenSpan
import link.desync.vnote.telemetry.SpanContext
import link.desync.vnote.telemetry.Telemetry
import java.util.UUID

// Drives one open page's ink channel: connects the WSS, replays persisted
// strokes (gap-fill), commits new strokes, and applies other devices' strokes,
// erasures and paper live as they arrive — any number of devices may edit one
// page at once (#630). Exposes Compose state for the canvas. Always-online MVP — a full
// disconnect/error UX lands in a later card — but it does reconnect on its own
// after any close it did not ask for (#279), resubscribing from its cursor.
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
) {
    // The page's paper. Seeded from the library listing so the first frame is
    // not blank, then WSS-authoritative: `Welcome` carries it on every connect
    // and `PaperChanged` carries each change.
    //
    // Backed by a private state property rather than `var paper … private set`
    // because that would compile to a private `setPaper(Paper)` and clash with
    // the public `setPaper` below on the same JVM signature.
    private var paperState by mutableStateOf(initialPaper)

    val paper: Paper get() = paperState

    // The last value the server confirmed, so an optimistic set can be reverted
    // if the server rejects it.
    private var confirmedPaper: Paper = initialPaper

    // The paper this session has asked for and not yet seen confirmed. Cleared
    // on confirmation and on every revert path, so a change that never landed
    // cannot later be mistaken for an acknowledgement.
    private var pendingPaper: Paper? = null

    // Rendered strokes: confirmed server batches followed by locally submitted
    // batches that are awaiting their matching server echo.
    var strokes by mutableStateOf<List<Stroke>>(emptyList())
        private set

    internal val pendingBatchCount: Int
        get() = pendingBatches.size

    // True while this session's socket is live and caught up: welcomed, and
    // its `subscribe` replay applied. Edits made otherwise are held (ink) or
    // not offered (erase, paper).
    var canEdit by mutableStateOf(false)
        private set

    // Non-null when there is something to surface to the user (#150): loading,
    // disconnected, retrying a failed commit, session expired, or a definitive
    // rejection. Null means sync is nominal.
    var status by mutableStateOf<PageStatus?>(null)
        private set

    // The banner's line, for callers that only need the copy.
    val statusBanner: String? get() = status?.message

    private var sessionId: String? = null
    private var lastSeq: Long = 0
    private val seenBatchIds = mutableSetOf<String>()
    private val confirmedStrokes = mutableListOf<Stroke>()
    private val pendingBatches = linkedMapOf<String, List<Stroke>>()

    // Strokes the server has not yet acknowledged — never sent, or sent and
    // awaiting their echo. Leaving the page now would lose them.
    val unsentStrokeCount: Int
        get() = pendingBatches.values.sumOf { it.size }
    private val pendingErasures = linkedMapOf<String, Set<String>>()
    private var socket: PageSocket? = null

    // True from `subscribe` until the server's `synced`.
    //
    // The server replays one `stroke-batch` per stored batch and Android
    // commits one stroke per batch, so opening a page delivers one message per
    // stroke. Publishing each one repainted the whole page N times — the
    // stroke-by-stroke reveal — and cost O(N²) list work on the way. The
    // batches are still applied as they arrive; only the publish waits, so the
    // page paints once, complete.
    private var replaying = false
    private var replayBatches = 0
    private var replayStartedAt = 0L

    // Telemetry (#406). One trace per open page, rooted when it connects; every
    // span below is parented to [screen] explicitly, never to "the current"
    // screen, so work still in flight when the user leaves stays in this trace.
    private var screen: SpanContext = Telemetry.screen()
    private var connectSpan: OpenSpan? = null
    private var subscribeSpan: OpenSpan? = null
    private val strokeSpans = hashMapOf<String, StrokeSpans>()

    // Set by [disconnect]: the close that follows is ours, not a failure.
    @Volatile
    private var closing = false

    // Bumped for every socket this session opens and whenever one is retired, so
    // a late callback from a socket a reconnect replaced is ignored rather than
    // tearing down its successor.
    @Volatile
    private var socketGeneration = 0

    // Reconnect after any close this session did not ask for (#279). The server
    // closes a page subscriber that falls behind its fan-out and expects the
    // client to come back — as it does after a restart or a dropped network —
    // so without this an open page went dead until the user left and returned.
    private var reconnectJob: Job? = null
    private var reconnectAttempts = 0

    // Set when ink is held — drawn before the socket caught up, or while
    // disconnected — or a socket drops with ink or erasures still
    // unacknowledged, and consumed once the next socket has caught up. Both
    // commits are
    // idempotent on the server (keyed by `client_batch_id` / `client_mutation_id`,
    // a retry is re-echoed rather than stored twice), so re-sending is safe
    // whether or not the dropped socket's commit landed — and the user keeps
    // ink they drew rather than watching it vanish on a reconnect.
    private var resendPendingOnSync = false

    // A `commit_failed` is being retried (#150): the server is reachable but
    // could not store ink. Ink is blocked — new strokes would only pile up
    // behind the failing ones — while every pending batch is re-sent with the
    // reconnect backoff. Re-sending is idempotent on `client_batch_id`, so a
    // batch that did land is only re-echoed. Cleared once nothing is pending.
    private var commitRetryJob: Job? = null
    private var commitRetryAttempts = 0
    private var commitRetrying = false

    // Set once a 401 on the upgrade has been answered with a forced token
    // refresh; a second 401 before the next `Welcome` means the refresh did not
    // help and the session is gone.
    private var refreshedAfterUnauthorized = false

    // Why new ink cannot be held right now, or null when it can. Set when this
    // session learns its ink cannot be stored — the channel reported an error,
    // a commit is being retried, or the session expired — and cleared once ink
    // can flow again.
    private var inkRefused by mutableStateOf<String?>(null)

    // True while new ink would be dropped, so the canvas does not draw it.
    val inkBlocked: Boolean get() = inkRefused != null

    fun connect() {
        status = PageStatus(PageStatusKind.Connecting, CONNECTING)
        screen = Telemetry.startScreen("screen.page", listOf(Attribute("vnote.page_id", pageId)))
        open()
    }

    // The banner's Retry: reconnect now instead of waiting out the backoff, and
    // start a rejected or expired session over. Only meaningful while the page
    // is not live; a healthy channel is left alone.
    fun retryNow() {
        if (closing || status?.kind !in RETRYABLE) {
            return
        }
        AppLog.i(TAG, "page channel retry requested", context = screen)
        reconnectJob?.cancel()
        reconnectJob = null
        stopCommitRetry()
        refreshedAfterUnauthorized = false
        reconnectAttempts = 0
        inkRefused = null
        retireSocket()
        open()
    }

    // Opens a page socket. Everything the session has already confirmed —
    // strokes, `lastSeq`, seen batch ids — is kept, so a reconnect's
    // `subscribe(lastSeq)` asks only for what it missed and `Welcome` restores
    // the paper.
    private fun open() {
        val generation = ++socketGeneration
        connectSpan = Telemetry.span("realtime.connect", screen).attr("vnote.channel", "page")
        // The upgrade request's own `http.client` span is opened by the
        // interceptor under this screen, and its `traceparent` is what the
        // server parents the connection to.
        val opened =
            apiClient.openPageSocket(
                pageId,
                object : PageEventListener {
                    // Pinned to the main thread: these callbacks arrive on
                    // OkHttp's reader thread and every handler writes Compose
                    // state, which must not race the UI's snapshot reads
                    // (#465). The caller's scope may carry a test dispatcher.
                    override fun onEvent(event: PageEvent) {
                        scope.launch(Dispatchers.Main) { if (generation == socketGeneration) handle(event) }
                    }

                    override fun onError(message: String) {
                        scope.launch(Dispatchers.Main) { if (generation == socketGeneration) handleDisconnected(message) }
                    }

                    override fun onUnauthorized(message: String) {
                        scope.launch(Dispatchers.Main) { if (generation == socketGeneration) handleUnauthorized(message) }
                    }

                    override fun onClosed() {
                        scope.launch(Dispatchers.Main) {
                            if (generation == socketGeneration) handleDisconnected(CLOSED_DETAIL)
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
        if (pendingBatches.isNotEmpty()) {
            AppLog.w(
                TAG,
                "leaving the page discards unsent ink: strokes=$unsentStrokeCount",
                context = screen,
            )
        }
        reconnectJob?.cancel()
        reconnectJob = null
        stopCommitRetry()
        failStrokeSpans("page closed")
        socket?.close()
        socket = null
    }

    // Commit a freshly captured stroke; it enters committed state only once
    // the server echoes it back with its assigned seq.
    //
    // Ink is never refused just because the socket has not caught up yet
    // (#390): the replay lands a round trip after the page is drawn, and again
    // on every reconnect, and a pen that touches down in that window used to
    // lose its whole stroke. Such a stroke is held — drawn, and queued in
    // [pendingBatches] — and sent by [resendPending] once the replay is
    // applied. Only when this session knows its ink cannot be stored (the
    // channel failed, or the session expired) is the stroke dropped.
    fun commitStroke(stroke: Stroke) {
        if (stroke.points.isEmpty()) {
            return
        }
        inkRefused?.let { reason ->
            AppLog.i(TAG, "stroke discarded: $reason", context = screen)
            return
        }
        val clientBatchId = "batch_${UUID.randomUUID().toString().replace("-", "")}"
        val submitted = listOf(stroke)
        pendingBatches[clientBatchId] = submitted
        strokeSpans[clientBatchId] = StrokeSpans.open(screen, clientBatchId, stroke)
        publishRenderableStrokes()
        val activeSocket = socket
        if (canEdit && activeSocket != null) {
            activeSocket.commitBatch(clientBatchId, submitted)
        } else {
            resendPendingOnSync = true
            AppLog.i(TAG, "stroke held until the page channel catches up: pending=${pendingBatches.size}", context = screen)
        }
    }

    // Change the page's paper. Optimistic so the canvas repaints immediately;
    // the server's `PaperChanged` confirms it, while `paper_failed` or a
    // disconnect revert to the last confirmed value. Another device's change
    // arriving meanwhile simply wins if it lands later: paper is last arrival
    // wins, and every session applies each `PaperChanged` in server order.
    //
    // Durable side effects are deferred to [onPaperConfirmed] rather than run
    // here: a dispatched change can still be refused (`paper_failed`) or lost
    // to a disconnect, and a caller must not persist a choice the server never
    // put in force.
    fun setPaper(next: Paper) {
        if (!canEdit) {
            return
        }
        if (next == paper) {
            // Already in force server-side, so it is confirmed by definition —
            // there is nothing to send and nothing to wait for.
            onPaperConfirmed(next)
            return
        }
        val activeSocket = socket ?: return
        paperState = next
        pendingPaper = next
        activeSocket.setPaper("paper_${UUID.randomUUID().toString().replace("-", "")}", next)
    }

    fun eraseStrokes(strokeIds: Collection<String>) {
        if (!canEdit || strokeIds.isEmpty()) return
        val activeSocket = socket ?: return
        val uniqueIds = strokeIds.toSet()
        val clientMutationId = "erase_${UUID.randomUUID().toString().replace("-", "")}"
        pendingErasures[clientMutationId] = uniqueIds
        publishRenderableStrokes()
        activeSocket.commitTombstones(clientMutationId, uniqueIds.toList())
    }

    private fun handle(event: PageEvent) {
        when (event) {
            is PageEvent.Welcome -> onWelcome(event)
            is PageEvent.StrokeBatch -> onStrokeBatch(event)
            is PageEvent.PageReplay -> onPageReplay(event)
            is PageEvent.Synced -> onSynced(event)
            is PageEvent.TombstoneBatch -> onTombstoneBatch(event)
            is PageEvent.PaperChanged -> onPaperChanged(event)
            is PageEvent.Failure -> onFailure(event)
        }
    }

    private fun onWelcome(event: PageEvent.Welcome) {
        connectSpan?.end()
        connectSpan = null
        AppLog.i(TAG, "page channel connected", context = screen)
        sessionId = event.sessionId
        status = null
        reconnectAttempts = 0
        refreshedAfterUnauthorized = false
        // Authoritative on every (re)connect, which is what
        // self-corrects a value that went stale in the open library.
        paperState = event.paper
        confirmedPaper = event.paper
        // A fresh connection supersedes anything still in flight.
        pendingPaper = null
        if (!commitRetrying) {
            inkRefused = null
        }
        // Catch up on persisted ink; [onSynced] opens the page for editing.
        beginReplay()
        socket?.subscribe(lastSeq)
    }

    // Every committed batch, whoever drew it: this session's own echo, a
    // replayed one, or another device's, fanned out live while this user may be
    // mid-stroke. Applying it only changes [strokes] — the committed layer —
    // never the stroke under the nib, which the canvas owns until pen-up.
    // Stacking order is server arrival order: confirmed batches stack in the
    // order the page channel delivers them, which is the order the server
    // stored them, with this session's still-pending ink drawn on top until
    // its own echo puts it in place.
    private fun onStrokeBatch(event: PageEvent.StrokeBatch) {
        if (seenBatchIds.add(event.clientBatchId)) {
            confirmedStrokes.addAll(event.strokes)
            if (pendingBatches.remove(event.clientBatchId) != null) {
                strokeSpans.remove(event.clientBatchId)?.confirmed(event.seq)
                if (commitRetrying && pendingBatches.isEmpty()) {
                    commitRecovered()
                }
            }
            if (replaying) {
                replayBatches += 1
            }
            // One state assignment swaps optimistic ink for the
            // confirmed batch, so Compose cannot render a blank gap.
            // Held back mid-replay; `Synced` publishes the whole page.
            publishUnlessReplaying()
        }
        if (event.seq > lastSeq) {
            lastSeq = event.seq
        }
    }

    // One frame, applied through the same handlers the per-message frames used:
    // `replaying` holds back every intermediate publish, so the whole page is
    // painted once, by the `synced` this frame carries.
    private fun onPageReplay(event: PageEvent.PageReplay) {
        for (batch in event.batches) {
            onStrokeBatch(batch)
        }
        for (tombstone in event.tombstones) {
            onTombstoneBatch(tombstone)
        }
        onSynced(PageEvent.Synced(event.lastSeq))
    }

    private fun onSynced(event: PageEvent.Synced) {
        if (event.lastSeq > lastSeq) {
            lastSeq = event.lastSeq
        }
        endReplay()
        onCaughtUp()
    }

    private fun onTombstoneBatch(event: PageEvent.TombstoneBatch) {
        val locallyErasedIds = pendingErasures.remove(event.clientMutationId).orEmpty()
        val deletedIds = locallyErasedIds + event.strokeIds
        confirmedStrokes.removeAll { it.id in deletedIds }
        pendingBatches.replaceAll { _, strokes -> strokes.filterNot { it.id in deletedIds } }
        // A batch erased before its echo will never be confirmed: close its
        // spans now, as what happened, rather than as "page closed" later.
        pendingBatches.filterValues { it.isEmpty() }.keys.forEach { erased ->
            strokeSpans.remove(erased)?.failed("erased")
        }
        pendingBatches.entries.removeAll { (_, strokes) -> strokes.isEmpty() }
        publishUnlessReplaying()
    }

    private fun onPaperChanged(event: PageEvent.PaperChanged) {
        paperState = event.paper
        confirmedPaper = event.paper
        // Only a change this session requested is reported as confirmed.
        // Matching on the value is sufficient: if the paper now in force
        // is the one asked for, that request has been satisfied — even
        // if a sibling device happened to set the same value first.
        if (pendingPaper == event.paper) {
            pendingPaper = null
            onPaperConfirmed(event.paper)
        }
    }

    // The socket is welcomed and its replay applied: edits flow from here on.
    private fun onCaughtUp() {
        if (socket == null) {
            return
        }
        canEdit = true
        if (resendPendingOnSync) {
            resendPendingOnSync = false
            resendPending()
        }
    }

    // Commit what is still pending once the socket has caught up: strokes
    // drawn before it did (#390), and what a previous socket never
    // acknowledged (#279). The replay has already been applied, so a batch the
    // server did store has been confirmed and removed from [pendingBatches],
    // and ink a replayed tombstone erased has been dropped from it. What is
    // left is exactly what the server may never have seen, in the order it was
    // drawn.
    private fun resendPending() {
        val activeSocket = socket ?: return
        if (pendingBatches.isEmpty() && pendingErasures.isEmpty()) return
        AppLog.i(
            TAG,
            "sending held or unacknowledged edits now the page channel has caught up: " +
                "batches=${pendingBatches.size} erasures=${pendingErasures.size}",
            context = screen,
        )
        pendingBatches.forEach { (clientBatchId, strokes) -> activeSocket.commitBatch(clientBatchId, strokes) }
        pendingErasures.forEach { (clientMutationId, ids) -> activeSocket.commitTombstones(clientMutationId, ids.toList()) }
    }

    private fun onFailure(event: PageEvent.Failure) {
        // The code only: the message is the server's, shown to the user as is.
        AppLog.w(
            TAG,
            "page channel error",
            context = screen,
            attributes = listOf(Attribute("vnote.error_code", event.code)),
        )
        if (event.code == COMMIT_FAILED) {
            onCommitFailed(event)
            return
        }
        // A replay that ends in an error still shows whatever arrived
        // before it — `replay_failed` must not leave the page blank.
        endReplay(failure = event.code)
        if (event.code == TOMBSTONE_FAILED) {
            restorePendingErasure(event.clientMutationId)
        }
        if (event.code == PAPER_FAILED) {
            revertUnconfirmedPaper()
        }
        canEdit = false
        stopCommitRetry()
        discardPendingBatches()
        resendPendingOnSync = false
        inkRefused = event.message
        status = PageStatus(PageStatusKind.Rejected, event.message, detail = event.code)
    }

    // The server is up but could not store a batch. Nothing is discarded: the
    // batch was not persisted (the server says so by sending this rather than
    // an echo), and every pending batch is re-sent until the server takes them.
    private fun onCommitFailed(event: PageEvent.Failure) {
        commitRetrying = true
        inkRefused = SYNC_RETRYING
        status =
            PageStatus(
                PageStatusKind.Retrying,
                SYNC_RETRYING,
                detail = "${event.code}: ${event.message}",
            )
        if (commitRetryJob?.isActive == true) {
            return
        }
        val delayMs = reconnectDelayMs(commitRetryAttempts++)
        AppLog.i(TAG, "retrying failed commit: pending=${pendingBatches.size} delayMs=$delayMs", context = screen)
        commitRetryJob =
            scope.launch {
                delay(delayMs)
                val activeSocket = socket ?: return@launch
                if (closing) return@launch
                if (canEdit) {
                    pendingBatches.forEach { (clientBatchId, strokes) -> activeSocket.commitBatch(clientBatchId, strokes) }
                } else {
                    // Not caught up yet: the replay's end sends them.
                    resendPendingOnSync = true
                }
            }
    }

    // Every batch the retry re-sent has been echoed: the server is storing ink
    // again, so the canvas takes it again.
    private fun commitRecovered() {
        AppLog.i(TAG, "failed commit recovered after attempts=$commitRetryAttempts", context = screen)
        stopCommitRetry()
        if (status?.kind == PageStatusKind.Retrying) {
            status = null
        }
    }

    private fun stopCommitRetry() {
        commitRetryJob?.cancel()
        commitRetryJob = null
        commitRetryAttempts = 0
        if (commitRetrying) {
            commitRetrying = false
            if (inkRefused == SYNC_RETRYING) {
                inkRefused = null
            }
        }
    }

    private fun beginReplay() {
        replaying = true
        replayBatches = 0
        replayStartedAt = SystemClock.elapsedRealtime()
        subscribeSpan = Telemetry.span("realtime.subscribe", screen).attr("vnote.channel", "page")
    }

    // Close the replay window and paint what arrived. Idempotent, because the
    // window can be closed by `synced`, by an error, or by the socket dropping.
    // [failure] says why it closed early, and fails the subscribe span.
    private fun endReplay(failure: String? = null) {
        if (!replaying) {
            return
        }
        replaying = false
        val span = subscribeSpan?.attr("vnote.replay.batches", replayBatches)
        subscribeSpan = null
        AppLog.d(
            TAG,
            "page replay applied: batches=$replayBatches " +
                "durationMs=${SystemClock.elapsedRealtime() - replayStartedAt}",
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
        val erasedIds = pendingErasures.values.flatten().toSet()
        strokes =
            (confirmedStrokes + pendingBatches.values.flatten())
                .filterNot { it.id in erasedIds }
    }

    private fun restorePendingErasure(clientMutationId: String?) {
        if (clientMutationId == null) {
            pendingErasures.clear()
        } else {
            pendingErasures.remove(clientMutationId)
        }
        publishRenderableStrokes()
    }

    private fun discardPendingBatches() {
        failStrokeSpans("discarded")
        if (pendingBatches.isNotEmpty()) {
            pendingBatches.clear()
            publishRenderableStrokes()
        }
    }

    private fun handleDisconnected(message: String) {
        if (!closing) {
            AppLog.w(TAG, "page channel disconnected: $message", context = screen)
        }
        connectSpan?.fail(message)
        connectSpan = null
        // A socket that drops mid-replay never sends `synced`, so publish the
        // partial page rather than withholding it until the next connect.
        endReplay(failure = "disconnected")
        canEdit = false
        stopCommitRetry()
        // Ink drawn until the next socket says otherwise is held for it.
        inkRefused = null
        revertUnconfirmedPaper()
        status = PageStatus(PageStatusKind.Disconnected, DISCONNECTED, detail = message)
        if (!closing) {
            if (pendingBatches.isNotEmpty() || pendingErasures.isNotEmpty()) {
                resendPendingOnSync = true
            }
            scheduleReconnect()
        }
    }

    // The upgrade was refused with 401. One forced token refresh, then straight
    // back in; a second refusal before the next `Welcome` means the session is
    // gone, and retrying on a timer would only be refused again.
    private fun handleUnauthorized(message: String) {
        AppLog.w(TAG, "page channel refused the session: $message", context = screen)
        connectSpan?.fail(message)
        connectSpan = null
        endReplay(failure = "unauthorized")
        canEdit = false
        stopCommitRetry()
        revertUnconfirmedPaper()
        retireSocket()
        if (closing) return
        if (pendingBatches.isNotEmpty() || pendingErasures.isNotEmpty()) {
            resendPendingOnSync = true
        }
        if (refreshedAfterUnauthorized) {
            sessionExpired(SESSION_EXPIRED, detail = message)
            return
        }
        refreshedAfterUnauthorized = true
        reconnectJob =
            scope.launch {
                val refreshed = apiClient.refreshSession()
                if (closing) return@launch
                if (refreshed) {
                    AppLog.i(TAG, "token refreshed; reconnecting the page channel", context = screen)
                    open()
                } else {
                    sessionExpired(SESSION_EXPIRED, detail = message)
                }
            }
    }

    // Nothing this session can do fixes the session itself: block ink — it
    // could never be sent — and say so. Ink already held stays held, so a
    // Retry after signing in elsewhere still sends it.
    private fun sessionExpired(
        message: String,
        detail: String?,
    ) {
        AppLog.w(TAG, "page channel stopped: session expired", context = screen)
        inkRefused = message
        status = PageStatus(PageStatusKind.SessionExpired, message, detail = detail)
    }

    // Retire the current socket: its late callbacks are ignored, and one that
    // reported an error without closing is closed rather than leaked.
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
        val delayMs = reconnectDelayMs(reconnectAttempts++)
        reconnectJob =
            scope.launch {
                delay(delayMs)
                if (!closing) {
                    // The disconnected banner stays up through the attempt: a
                    // reconnect is not "connecting" to the user until it has
                    // worked, and `Welcome` is what clears it.
                    AppLog.i(TAG, "page channel reconnecting", context = screen)
                    open()
                }
            }
    }

    private fun failStrokeSpans(reason: String) {
        strokeSpans.values.forEach { it.failed(reason) }
        strokeSpans.clear()
    }

    // Drop an optimistic paper the server never acknowledged: `paper_failed`,
    // or the socket closing before any reply.
    // Without this the canvas would keep showing paper that was never stored,
    // until the next `Welcome` reasserted the authoritative value.
    private fun revertUnconfirmedPaper() {
        pendingPaper = null
        if (paperState != confirmedPaper) {
            paperState = confirmedPaper
        }
    }

    companion object {
        // First retry after 1 s — the SPA's page loop and the library channel
        // wait the same — doubling to a 30 s ceiling while the server stays
        // unreachable. Reset by the next `Welcome`.
        internal fun reconnectDelayMs(attempt: Int): Long =
            (RECONNECT_BASE_MS shl attempt.coerceIn(0, RECONNECT_MAX_DOUBLINGS)).coerceAtMost(RECONNECT_MAX_MS)

        private const val RECONNECT_BASE_MS = 1_000L
        private const val RECONNECT_MAX_MS = 30_000L
        private const val RECONNECT_MAX_DOUBLINGS = 5
        private const val TAG = "PageInkSession"
        private const val TOMBSTONE_FAILED = "tombstone_failed"
        private const val PAPER_FAILED = "paper_failed"
        private const val COMMIT_FAILED = "commit_failed"
        private const val CLOSED_DETAIL = "connection closed"
        private val RETRYABLE =
            setOf(PageStatusKind.Disconnected, PageStatusKind.SessionExpired, PageStatusKind.Rejected)

        // Copy, one line per kind of trouble (#150) so they never read alike:
        // no connection, the server failing to store ink, and a session only
        // signing in again can fix.
        const val CONNECTING = "Connecting…"
        const val DISCONNECTED = "Disconnected from the server — reconnecting"
        const val SYNC_RETRYING = "Couldn't save your ink — retrying"
        const val SESSION_EXPIRED = "Your session has expired — sign in again"
        const val NOT_SIGNED_IN = "Not signed in"
    }
}
