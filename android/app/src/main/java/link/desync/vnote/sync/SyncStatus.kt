package link.desync.vnote.sync

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import link.desync.vnote.api.AuthUnavailableException
import link.desync.vnote.api.HttpStatusException
import link.desync.vnote.api.SessionExpiredException
import link.desync.vnote.store.LocalStore
import link.desync.vnote.store.OpKind

// Why the last attempt to reach a host failed (#159): which host, what
// failed, and the request's correlation id for support. Never content.
data class ConnectionProblem(
    val host: String,
    val errorClass: String,
    val correlationId: String?,
    val atMillis: Long,
)

// What the top-bar icon shows. Quiet means online. Offline and sign-in needed
// wear the same icon (#651); they differ only in what the details offer.
enum class SyncIndicator(
    // The icon's test tag suffix.
    val tag: String,
) {
    Synced("synced"),
    Offline("offline"),
    SignInNeeded("sign-in"),
}

// A page the server says was permanently deleted (410, or `page_deleted` on
// its channel) while this device still owes it edits (#651). They stay owed
// and are retried like everything else; the details also offer to keep them
// as a new page, since on that page they can never land.
data class GonePage(
    val pageId: String,
    val strokeCount: Int,
)

// The app's one sync status (#159), shown on every screen: whether the last
// attempt to sync failed and how, whether there is a token to sync with, and
// how many edits are waiting. Fed by the library channel, every page session
// and the sync engine; read by the status icon. Compose state, written on
// the main thread only.
class SyncStatus(
    private val store: LocalStore,
    private val clock: () -> Long = System::currentTimeMillis,
) {
    var problem by mutableStateOf<ConnectionProblem?>(null)
        private set

    // There is no token to sync with (#651): the identity provider refused
    // the refresh (`invalid_grant`), or the user signed out. Editing carries
    // on; sync waits for a sign-in. Nothing else sets it.
    var signInNeeded by mutableStateOf(false)
        private set

    var pendingCount by mutableStateOf(0)
        private set

    var gonePages by mutableStateOf<List<GonePage>>(emptyList())
        private set

    var lastSyncAt by mutableStateOf<Long?>(null)
        private set

    // Pages last answered with 410; [gonePages] is those that still owe edits.
    private val goneIds = linkedSetOf<String>()

    // Shown only when the last attempt failed or there is no token — never
    // just because edits are pending (#651): while online they are on their
    // way, and an icon per stroke is noise. A permanently deleted page keeps
    // it up, so the offer to keep its edits stays within reach.
    val indicator: SyncIndicator
        get() =
            when {
                signInNeeded -> SyncIndicator.SignInNeeded
                problem != null || gonePages.isNotEmpty() -> SyncIndicator.Offline
                else -> SyncIndicator.Synced
            }

    // Re-reads the counts after the store changed.
    fun refresh() {
        pendingCount = store.outbox.read.unsyncedCount()
        lastSyncAt = store.identity.lastSyncAt()
        refreshGone()
    }

    fun reportFailure(
        host: String,
        errorClass: String,
        correlationId: String?,
    ) {
        problem = ConnectionProblem(host, errorClass, correlationId, clock())
    }

    // A failed REST call, whatever it was (#651). Every failure is reported
    // alike and retried by its caller; the one exception is having no token,
    // which only a sign-in fixes.
    fun reportFailure(
        failure: Throwable?,
        serverHost: String,
        signInHost: String,
    ) {
        when (failure) {
            is SessionExpiredException -> requireSignIn()
            is AuthUnavailableException -> reportFailure(signInHost, ErrorClass.OFFLINE, null)
            is HttpStatusException -> reportFailure(serverHost, ErrorClass.http(failure.code), failure.requestId)
            is java.io.IOException -> reportFailure(serverHost, ErrorClass.OFFLINE, null)
            // A 2xx whose body is not what v-note sends (a captive portal's
            // page, a proxy's), or anything unforeseen: named by its class.
            else -> reportFailure(serverHost, failure?.javaClass?.simpleName ?: ErrorClass.UNKNOWN, null)
        }
    }

    // Sync worked — the server answered with the token in hand — so nothing
    // stands in its way now: a provider out of reach does not matter while
    // the access token is valid (#159).
    fun reportReachable() {
        problem = null
    }

    // The server's last answer for [pageId] was "permanently deleted".
    fun reportGone(pageId: String) {
        if (goneIds.add(pageId)) refreshGone()
    }

    // The server answered for [pageId] as a page it holds.
    fun reportPresent(pageId: String) {
        if (goneIds.remove(pageId)) refreshGone()
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

    private fun refreshGone() {
        val owing =
            goneIds.mapNotNull { pageId ->
                val owed =
                    store.outbox.read
                        .forPage(pageId)
                        .filter { it.kind != OpKind.CreatePage }
                if (owed.isEmpty()) null else GonePage(pageId, owed.filter { it.kind == OpKind.CommitBatch }.sumOf { it.strokes.size })
            }
        // Nothing owed any more — kept as a new page, or settled: forgotten.
        goneIds.retainAll(owing.map { it.pageId }.toSet())
        if (owing != gonePages) gonePages = owing
    }
}

// What the diagnostics name a failure by (#159): an HTTP status, the page
// channel's own error code, an exception's class, or one of these words.
object ErrorClass {
    const val OFFLINE = "offline"
    const val TIMEOUT = "timeout"
    const val UNKNOWN = "unknown"

    // A 2xx whose page id is not the one this device chose.
    const val ID_MISMATCH = "id_mismatch"

    // The page channel's error code for a permanently deleted page (#631).
    const val PAGE_DELETED = "page_deleted"

    private const val HTTP_GONE = 410

    fun http(code: Int): String = "HTTP $code"

    // Whether [errorClass] is the server saying the page is permanently
    // deleted: a 410 on its upgrade, or `page_deleted` on its open channel.
    fun isGone(errorClass: String): Boolean = errorClass == PAGE_DELETED || errorClass == http(HTTP_GONE)
}
