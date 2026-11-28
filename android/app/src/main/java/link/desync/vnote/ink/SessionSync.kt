package link.desync.vnote.ink

import androidx.compose.runtime.compositionLocalOf
import link.desync.vnote.store.LocalStore
import link.desync.vnote.sync.SyncEngine
import link.desync.vnote.sync.SyncStatus

// How a [PageInkSession] takes part in local-first sync (#159): the store it
// keeps the page's books in, the status it reports to, and — for a session
// the sync engine runs with no screen — when it is done.
class SessionSync(
    val store: LocalStore,
    val status: SyncStatus? = null,
    // No screen: the sync engine opened this session to replay the page's
    // outbox or fetch its ink, and closes it once [onIdle] fires.
    val headless: Boolean = false,
    // Nothing left this session can do now: caught up with nothing owed, or
    // failed — a headless session leaves the retry to the sync engine.
    val onIdle: () -> Unit = {},
    // True while the page was made on this device and the server has not
    // acknowledged its create: there is no page channel to open yet.
    val createPending: () -> Boolean = { false },
)

// What an open page needs of local-first sync, provided once by the activity
// so the page screen's parameters stay the screen's own.
class PageSyncHost(
    val store: LocalStore,
    val status: SyncStatus,
    val engine: SyncEngine,
) {
    fun sessionSync(pageId: String): SessionSync = SessionSync(store, status, createPending = { store.library.isLocalOnly(pageId) })
}

val LocalPageSync = compositionLocalOf<PageSyncHost?> { null }
