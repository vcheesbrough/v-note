package link.desync.vnote.sync

import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import link.desync.vnote.api.ApiClient
import link.desync.vnote.ink.PageInkSession
import link.desync.vnote.ink.SessionSync
import link.desync.vnote.model.PageSummary
import link.desync.vnote.store.LocalStore
import link.desync.vnote.telemetry.AppLog

// Catches the device up with the server, with no user action (#159): pushes
// the pages created here, replays every page's outbox, and fetches the ink of
// every library page that changed, so each page opens offline. A page is
// replayed or fetched by a headless [PageInkSession] — the same code an open
// page runs, with no screen — and the page the user has open is left to its
// own session. Runs on the main thread; kicked whenever the network may be
// back (the library channel opening), after every edit that has nowhere to
// go, and on a backoff while creates wait.
class SyncEngine(
    private val apiClient: ApiClient,
    private val store: LocalStore,
    private val status: SyncStatus,
    private val scope: CoroutineScope,
) {
    private val creates = PageCreates(apiClient, store, status)
    val recovery = PageRecovery(store)

    // A page the server now holds, for the library to show as the server has it.
    var onPageCreated: (PageSummary) -> Unit = {}

    private val background = linkedMapOf<String, PageInkSession>()
    private var focusedPageId: String? = null
    private var focused: PageInkSession? = null
    private var running = false
    private var draining: Job? = null
    private var rerun = false
    private var retryJob: Job? = null
    private var retryAttempts = 0

    // Pages whose fetch ended in a definitive refusal this run: not fetched
    // again until the next start, so a refusal cannot spin.
    private val refusedFetches = hashSetOf<String>()

    // Signed in (from the device or freshly): sync may run.
    fun start() {
        running = true
        refusedFetches.clear()
        store.outbox.pruneAcknowledged()
        status.refresh()
        // A page left waiting for a sign-in picks up again by itself (#159).
        focused?.retryNow()
        kick()
    }

    // Signed out, or a different identity is taking over: stop everything.
    fun stop() {
        running = false
        retryJob?.cancel()
        retryJob = null
        draining?.cancel()
        draining = null
        background.values.forEach { it.disconnect() }
        background.clear()
    }

    // Look for work now. Cheap and idempotent: a kick during a pass runs one
    // more pass after it.
    fun kick() {
        if (!running) return
        if (draining?.isActive == true) {
            rerun = true
            return
        }
        draining =
            scope.launch {
                do {
                    rerun = false
                    drain()
                } while (rerun && running)
            }
    }

    // The page now on screen, or null when none is: an open page syncs
    // itself, so a background session for it is closed first.
    fun focus(
        pageId: String?,
        session: PageInkSession?,
    ) {
        if (pageId != null) background.remove(pageId)?.disconnect()
        focusedPageId = pageId
        focused = session
        if (pageId == null) kick()
    }

    // Whether [pageId] was made here and is not on the server yet.
    fun createPending(pageId: String): Boolean = store.library.isLocalOnly(pageId)

    private suspend fun drain() {
        if (status.signInNeeded) return
        val createsDone =
            creates.push { page ->
                onPageCreated(page)
                if (page.id == focusedPageId) focused?.createAcknowledged()
            }
        if (createsDone) {
            retryAttempts = 0
        } else {
            scheduleRetry()
        }
        // Replays first: they hold the user's unsent ink. Fetching other
        // pages' ink waits for a free slot.
        // A page made here waits for its create; a refused page waits for the
        // user to recover it.
        val waiting =
            store.library.localOnlyIds() +
                store.outbox.read
                    .rejections()
                    .map { it.pageId }
        val replays =
            store.outbox.read
                .pagesWithWork()
                .filterNot { it in waiting }
        val fetches =
            if (createsDone) store.library.staleInk().filterNot { it.id in waiting || it.id in refusedFetches } else emptyList()
        for (pageId in replays) startSession(pageId, markSynced = null)
        for (page in fetches) startSession(page.id, markSynced = page.updatedAt)
    }

    private fun startSession(
        pageId: String,
        markSynced: String?,
    ) {
        if (background.size >= MAX_BACKGROUND || pageId == focusedPageId || pageId in background) return
        lateinit var session: PageInkSession
        val sync =
            SessionSync(
                store = store,
                status = status,
                headless = true,
                onIdle = { scope.launch { finish(pageId, session, markSynced) } },
                createPending = { createPending(pageId) },
            )
        session = PageInkSession(apiClient, pageId, scope, sync = sync)
        background[pageId] = session
        AppLog.d(TAG, "background sync started: fetch=${markSynced != null} running=${background.size}")
        session.connect()
    }

    // A background session has nothing more to do now. A fetch that caught up
    // records that the page's ink is current; one stopped by a rejection or an
    // expired session does not.
    private fun finish(
        pageId: String,
        session: PageInkSession,
        markSynced: String?,
    ) {
        if (background[pageId] !== session) return
        background.remove(pageId)
        val caughtUp = session.canEdit
        session.disconnect()
        if (markSynced != null) {
            if (caughtUp) store.library.markInkSynced(pageId, markSynced) else refusedFetches += pageId
        }
        // A pass after a session stopped for a sign-in finds nothing to do
        // until the user signs in; one after a refusal skips the page.
        kick()
    }

    private fun scheduleRetry() {
        if (retryJob?.isActive == true) return
        val delayMs = PageInkSession.reconnectDelayMs(retryAttempts++)
        retryJob =
            scope.launch {
                delay(delayMs)
                kick()
            }
    }

    private companion object {
        const val TAG = "VNoteSync"

        // Background sockets at once: enough to drain a backlog quickly
        // without crowding the open page's own channel.
        const val MAX_BACKGROUND = 3
    }
}
