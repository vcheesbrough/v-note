package link.desync.vnote.sync

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import link.desync.vnote.store.LocalStore
import link.desync.vnote.store.Rejection

// Why the last attempt to reach a host failed (#159): which host, what kind
// of failure, and the request's correlation id for support. Never content.
data class ConnectionProblem(
    val host: String,
    val errorClass: String,
    val correlationId: String?,
    val atMillis: Long,
)

// What the top-bar icon shows, worst first. Quiet means healthy.
enum class SyncIndicator(
    // The icon's test tag suffix.
    val tag: String,
) {
    Synced("synced"),
    Offline("offline"),
    SignInNeeded("sign-in"),
    Rejected("rejected"),
}

// The app's one sync status (#159), shown on every screen: whether the
// server and the identity provider can be reached, whether sync is paused
// for a sign-in, how many edits are waiting, and what was refused. Fed by
// the library channel, every page session and the sync engine; read by the
// status icon. Compose state, written on the main thread only.
class SyncStatus(
    private val store: LocalStore,
    private val clock: () -> Long = System::currentTimeMillis,
) {
    var problem by mutableStateOf<ConnectionProblem?>(null)
        private set

    // A refresh was refused (`invalid_grant`), the session expired, or the
    // user signed out: editing carries on, sync waits for a sign-in.
    var signInNeeded by mutableStateOf(false)
        private set

    var pendingCount by mutableStateOf(0)
        private set

    var rejections by mutableStateOf<List<Rejection>>(emptyList())
        private set

    var lastSyncAt by mutableStateOf<Long?>(null)
        private set

    val indicator: SyncIndicator
        get() =
            when {
                rejections.isNotEmpty() -> SyncIndicator.Rejected
                signInNeeded -> SyncIndicator.SignInNeeded
                problem != null || pendingCount > 0 -> SyncIndicator.Offline
                else -> SyncIndicator.Synced
            }

    // Re-reads the counts after the store changed.
    fun refresh() {
        pendingCount = store.outbox.read.awaitingCount()
        rejections = store.outbox.read.rejections()
        lastSyncAt = store.identity.lastSyncAt()
    }

    fun reportFailure(
        host: String,
        errorClass: String,
        correlationId: String?,
    ) {
        problem = ConnectionProblem(host, errorClass, correlationId, clock())
    }

    // Sync worked — the server answered with the token in hand — so nothing
    // stands in its way now: a provider out of reach does not matter while
    // the access token is valid (#159).
    fun reportReachable() {
        problem = null
    }

    fun requireSignIn() {
        signInNeeded = true
    }

    fun signedIn() {
        signInNeeded = false
    }

    // The server acknowledged an edit or served a fresh snapshot.
    fun markSynced() {
        val now = clock()
        store.identity.markSynced(now)
        lastSyncAt = now
    }
}

// Failure classes the diagnostics name (#159). One word each, so they read
// alike on every screen and in support requests.
object ErrorClass {
    const val OFFLINE = "offline"
    const val TIMEOUT = "timeout"
    const val SESSION_EXPIRED = "session-expired"
    const val SERVER_ERROR = "server-error"

    fun http(code: Int): String = "http-$code"
}
