package link.desync.vnote.sync

import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
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
// own session. Runs on the main thread.
//
// Nothing is ever given up on (#651). Whatever fails — a create, a replay, a
// fetch, for any reason — stays owed and is tried again on the [Backoff]
// schedule, with no last attempt, and at once when the network returns
// ([retryNow]). A page that failed gives its slot back and waits its turn, so
// one page that can never succeed (permanently deleted elsewhere) does not
// stand in the way of the rest.
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

    private val retry = RetryTimer(scope)

    // Pages whose last background attempt failed, oldest failure first. They
    // are tried again only on a retry, a few at a time and each at most once
    // ([probing]), so a server that is down is asked a few times per backoff
    // step however many pages are waiting. One of them succeeding means it
    // is back: all go.
    private val failed = linkedSetOf<String>()

    // The failed pages this retry may still try. A page leaves it when its
    // attempt starts, so a later pass in the same step cannot try it again.
    private val probing = linkedSetOf<String>()
    private var createsFailing = false

    // Signed in (from the device or freshly): sync may run.
    fun start() {
        running = true
        store.outbox.pruneAcknowledged()
        status.refresh()
        retryNow()
    }

    // Signed out, or a different identity is taking over: stop everything.
    fun stop() {
        running = false
        retry.cancel()
        draining?.cancel()
        draining = null
        background.values.forEach { it.disconnect() }
        background.clear()
        failed.clear()
        probing.clear()
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

    // The network may be back — a network became available, the library
    // channel opened, the user signed in: try everything now instead of
    // waiting out the backoff, which starts over. The open page too.
    fun retryNow() {
        if (!running) return
        recovered()
        focused?.retryNow()
        kick()
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

    private suspend fun drain() {
        // No token: nothing to try with until the user signs in.
        if (status.signInNeeded) return
        val createsDone =
            creates.push { page ->
                onPageCreated(page)
                if (page.id == focusedPageId) focused?.createAcknowledged()
            }
        if (!createsDone) {
            createsFailing = true
            scheduleRetry()
        } else if (createsFailing) {
            recovered()
        }
        // A page made here waits for its create: it has no channel yet.
        val waiting = store.library.localOnlyIds()
        // Replays first: they hold the user's unsent ink. Fetching other
        // pages' ink waits for a free slot.
        val wanted = linkedMapOf<String, String?>()
        store.outbox.read
            .pagesWithWork()
            .filterNot { it in waiting }
            .forEach { wanted[it] = null }
        store.library
            .staleInk()
            .filterNot { it.id in waiting || it.id in wanted }
            .forEach { wanted[it.id] = it.updatedAt }
        // A failed page with nothing left to do has nothing to retry.
        failed.retainAll(wanted.keys)
        probing.retainAll(failed)
        for ((pageId, markSynced) in wanted) {
            if (pageId !in failed) startSession(pageId, markSynced)
        }
        for (pageId in probing.toList()) {
            if (startSession(pageId, wanted[pageId])) probing.remove(pageId)
        }
    }

    // True when a session was started for the page.
    private fun startSession(
        pageId: String,
        markSynced: String?,
    ): Boolean {
        if (background.size >= MAX_BACKGROUND || pageId == focusedPageId || pageId in background) return false
        lateinit var session: PageInkSession
        val sync =
            SessionSync(
                store = store,
                status = status,
                headless = true,
                onIdle = { scope.launch { finish(pageId, session, markSynced) } },
                createPending = { store.library.isLocalOnly(pageId) },
            )
        session = PageInkSession(apiClient, pageId, scope, sync = sync)
        background[pageId] = session
        AppLog.d(TAG, "background sync started: fetch=${markSynced != null} running=${background.size}")
        session.connect()
        return true
    }

    // A background session has nothing more to do now: it caught up with
    // nothing owed, or it failed. Either way its slot is free. A fetch that
    // caught up records that the page's ink is current. A session that
    // failed did not: its page is tried again on the backoff.
    private fun finish(
        pageId: String,
        session: PageInkSession,
        markSynced: String?,
    ) {
        if (background[pageId] !== session) return
        background.remove(pageId)
        val caughtUp = session.canEdit
        session.disconnect()
        if (caughtUp) {
            if (markSynced != null) store.library.markInkSynced(pageId, markSynced)
            // A page that had failed got through: the server is back.
            if (pageId in failed) recovered()
        } else {
            // To the back of the queue, so every failed page gets its turn.
            failed.remove(pageId)
            failed.add(pageId)
            scheduleRetry()
        }
        kick()
    }

    // Something that had been failing worked, or the network returned:
    // everything waiting is tried now, and the backoff starts over.
    private fun recovered() {
        failed.clear()
        probing.clear()
        createsFailing = false
        retry.startOver()
    }

    // One retry, however many things are waiting for it: a few of the
    // failed pages are tried again, and the creates with them.
    private fun scheduleRetry() =
        retry.schedule {
            probing.clear()
            probing.addAll(failed.take(MAX_BACKGROUND))
            kick()
        }

    private companion object {
        const val TAG = "VNoteSync"

        // Background sockets at once: enough to drain a backlog quickly
        // without crowding the open page's own channel.
        const val MAX_BACKGROUND = 3
    }
}
