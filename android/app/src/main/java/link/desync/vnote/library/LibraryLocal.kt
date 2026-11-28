package link.desync.vnote.library

import link.desync.vnote.api.AuthUnavailableException
import link.desync.vnote.api.ConnectionFailure
import link.desync.vnote.api.HttpStatusException
import link.desync.vnote.api.SessionExpiredException
import link.desync.vnote.store.LocalStore
import link.desync.vnote.sync.ErrorClass
import link.desync.vnote.sync.SyncStatus

// What the library holder needs of local-first sync (#159): the device's
// copy of the library, the sync status its failures go to, and a way to tell
// the sync engine there is work. [None] — no store, no status — is the
// library as it was before #159, which the JVM unit tests drive.
internal class LibraryLocal(
    val store: LocalStore?,
    val status: SyncStatus?,
    val serverHost: String,
    private val signInHost: String = serverHost,
    private val kick: () -> Unit = {},
) {
    fun onLocalEdit() {
        status?.refresh()
        kick()
    }

    // The library channel is open: the server is reachable.
    fun onOnline() {
        status?.reportReachable()
        kick()
    }

    fun onSnapshot() {
        status?.reportReachable()
        status?.markSynced()
    }

    fun onFailure(
        status: SyncStatus,
        failure: Throwable,
    ) {
        when (failure) {
            is SessionExpiredException -> status.requireSignIn()
            is AuthUnavailableException -> status.reportFailure(signInHost, ErrorClass.OFFLINE, null)
            is HttpStatusException -> status.reportFailure(serverHost, ErrorClass.http(failure.code), failure.requestId)
            is java.io.IOException -> status.reportFailure(serverHost, ErrorClass.OFFLINE, null)
            // "Not signed in": there is no token to sync with.
            else -> status.requireSignIn()
        }
    }

    fun failure(message: String): ConnectionFailure = ConnectionFailure(message, serverHost, ErrorClass.OFFLINE, null)

    companion object {
        val None = LibraryLocal(store = null, status = null, serverHost = "")
    }
}
