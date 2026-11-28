package link.desync.vnote.api

import link.desync.vnote.auth.RefreshOutcome
import link.desync.vnote.ink.Paper
import link.desync.vnote.model.LibraryEvent
import link.desync.vnote.model.MeProfile
import link.desync.vnote.model.PageEvent
import link.desync.vnote.model.PageSummary
import okhttp3.WebSocket

// The recycle bin's REST calls (#631). Split from [ApiClient], which extends
// it, so each interface stays one concern.
interface PageBinApi {
    // The binned pages, most recently binned first.
    suspend fun listBin(): Result<List<PageSummary>>

    // Brings a page back from the bin with all its ink.
    suspend fun restorePage(pageId: String): Result<PageSummary>

    // Permanently deletes one binned page — the only destructive step.
    suspend fun deletePagePermanently(pageId: String): Result<Unit>

    // Empties the bin: permanently deletes the binned pages named — the ones
    // the user was shown — so a page binned elsewhere since survives. Returns
    // the ids actually deleted.
    suspend fun emptyBin(pageIds: List<String>): Result<List<String>>
}

// The v-note server as the app sees it: the REST calls plus the two realtime
// channels. [OkHttpApiClient] is the production implementation; screens and the
// ink session depend on this interface so they can run against a fake.
interface ApiClient : PageBinApi {
    // The server this client talks to, as the menu names it.
    val baseUrl: String

    // The identity provider's host: a separate host the network may fail to
    // reach while the server answers (#159). Named in the sync diagnostics.
    val signInHost: String get() = hostOf(baseUrl)

    // One unauthenticated `GET /health`. Failure carries a short reason
    // ("HTTP 502", or the transport error) for the menu to show.
    suspend fun checkHealth(): Result<Unit>

    suspend fun fetchMe(): Result<MeProfile>

    suspend fun listPages(): Result<List<PageSummary>>

    // [id], when given, is the page's id minted on this device (#159): the
    // create is idempotent on it, so an offline create replayed after a lost
    // answer returns the page it already made.
    suspend fun createPage(
        title: String? = null,
        paper: Paper = Paper.None,
        id: String? = null,
    ): Result<PageSummary>

    // Moves the page to the bin (#631) — nothing is destroyed. The bin's own
    // calls are [PageBinApi]'s.
    suspend fun deletePage(pageId: String): Result<Unit>

    suspend fun fetchThumbnail(url: String): Result<ByteArray>

    // Owner-wide library channel: page created/binned/restored/deleted/updated
    // and thumbnail state. Null when there is no access token.
    fun openLibrarySocket(listener: LibraryEventListener): WebSocket?

    // Per-page ink channel. Bidirectional: the caller subscribes for gap-fill
    // and commits coalesced stroke batches, erasures and paper via the
    // returned [PageSocket]; server events — this device's echoes and every
    // other device's edits alike — arrive on [listener].
    fun openPageSocket(
        pageId: String,
        listener: PageEventListener,
    ): PageSocket?

    // Force a token refresh after the server refused the current one (a 401
    // on a WebSocket upgrade, which OkHttp cannot retry the way REST calls
    // are). True when a fresh token is now stored. A client with no session to
    // refresh — a test fake — refreshes nothing.
    suspend fun refreshSession(): Boolean = false

    // Why the last [refreshSession] returned false (#159): a refused refresh
    // leaves no token, so sync waits for a sign-in; an unreachable identity
    // provider is a failure like any other and is retried.
    // A client with no session to refresh — a test fake — was refused.
    val lastRefreshOutcome: RefreshOutcome get() = RefreshOutcome.Refused

    fun shutdown()
}

// The host an [ApiClient] request or socket failed to reach, for the sync
// diagnostics (#159): the backend, or the identity provider behind it.
fun hostOf(url: String): String = runCatching { java.net.URI(url).host }.getOrNull() ?: url

// A request answered with anything but a 2xx — by the server or by whatever
// stands in front of it (a proxy, a captive portal). Every [code] is the same
// failure (#651): retried until it succeeds. [code] is only what is shown.
class HttpStatusException(
    val code: Int,
    message: String,
    val requestId: String?,
) : IllegalStateException(message)

// The server refused the token and the identity provider could not be reached
// to refresh it. Not an expired session: sync waits for the provider (#159).
class AuthUnavailableException(
    host: String,
) : java.io.IOException("Can't reach the sign-in server ($host)")

// Why a realtime socket could not be used, with what the diagnostics show.
data class ConnectionFailure(
    val message: String,
    val host: String,
    val errorClass: String,
    // The `X-Request-Id` the failed upgrade carried, or the server's own.
    val correlationId: String?,
)

// There is no token to sync with (#651): none is stored, or the identity
// provider refused the refresh (`invalid_grant`). The one failure retrying
// cannot fix — only signing in again can. Nothing else raises it: a 401 that
// is still a 401 after a successful refresh is an [HttpStatusException] like
// any other status, and no failure ever signs the user out.
class SessionExpiredException : IllegalStateException("Session expired")

// The request never reached the server: no network (airplane mode), no route,
// DNS, a refused or timed-out connection. Says so in words a user can act on,
// with the transport's own message as the detail.
class ServerUnreachableException(
    cause: java.io.IOException,
) : java.io.IOException(
        "Can't reach the server (${cause.message ?: cause.javaClass.simpleName})",
        cause,
    )

interface LibraryEventListener {
    // The upgrade succeeded. The server subscribes a library socket before it
    // answers the upgrade (#631), so every event published from here on is
    // queued on it: a snapshot fetched after this misses nothing (#635).
    fun onOpen() = Unit

    fun onEvent(event: LibraryEvent)

    fun onError(message: String)

    // The socket failed; [failure] says where and why. Defaults to [onError].
    fun onConnectionFailure(failure: ConnectionFailure) = onError(failure.message)

    fun onClosed()
}

interface PageEventListener {
    fun onEvent(event: PageEvent)

    fun onError(message: String)

    // The socket failed; [failure] says where and why. Defaults to [onError].
    fun onConnectionFailure(failure: ConnectionFailure) = onError(failure.message)

    // The upgrade was refused with 401. Told apart from [onError] only so the
    // token is refreshed before the reconnect.
    fun onUnauthorized(message: String) = onError(message)

    fun onClosed()
}
