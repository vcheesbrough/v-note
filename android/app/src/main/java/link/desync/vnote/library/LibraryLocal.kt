package link.desync.vnote.library

import link.desync.vnote.api.ConnectionFailure
import link.desync.vnote.store.LocalStore
import link.desync.vnote.sync.ErrorClass
import link.desync.vnote.sync.SyncStatus

// What the library holder needs of local-first sync (#159): the device's
// copy of the library, the sync status its failures go to, and a way to tell
// the sync engine there is work, or that the server is back. [None] — no store, no status — is the
// library as it was before #159, which the JVM unit tests drive.
internal class LibraryLocal(
    val store: LocalStore?,
    val status: SyncStatus?,
    val serverHost: String,
    private val signInHost: String = serverHost,
    private val kick: () -> Unit = {},
    private val retryNow: () -> Unit = kick,
) {
    fun onLocalEdit() {
        status?.refresh()
        kick()
    }

    // The library channel is open: the server is reachable, so whatever
    // was waiting out a backoff is tried now.
    fun onOnline() {
        status?.reportReachable()
        retryNow()
    }

    fun onSnapshot() {
        status?.reportReachable()
        status?.markSynced()
    }

    // Whatever the failure, the library stays the device's copy and the
    // channel keeps reconnecting (#651).
    fun onFailure(
        status: SyncStatus,
        failure: Throwable,
    ) = status.reportFailure(failure, serverHost, signInHost)

    fun failure(message: String): ConnectionFailure = ConnectionFailure(message, serverHost, ErrorClass.OFFLINE, null)

    companion object {
        val None = LibraryLocal(store = null, status = null, serverHost = "")
    }
}
