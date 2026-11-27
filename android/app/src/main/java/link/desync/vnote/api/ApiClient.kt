package link.desync.vnote.api

import link.desync.vnote.ink.Paper
import link.desync.vnote.model.LibraryEvent
import link.desync.vnote.model.MeProfile
import link.desync.vnote.model.PageEvent
import link.desync.vnote.model.PageSummary
import okhttp3.WebSocket

// The v-note server as the app sees it: the REST calls plus the two realtime
// channels. [OkHttpApiClient] is the production implementation; screens and the
// ink session depend on this interface so they can run against a fake.
interface ApiClient {
    // The server this client talks to, as the menu names it.
    val baseUrl: String

    // One unauthenticated `GET /health`. Failure carries a short reason
    // ("HTTP 502", or the transport error) for the menu to show.
    suspend fun checkHealth(): Result<Unit>

    suspend fun fetchMe(): Result<MeProfile>

    suspend fun listPages(): Result<List<PageSummary>>

    suspend fun createPage(
        title: String? = null,
        paper: Paper = Paper.None,
    ): Result<PageSummary>

    suspend fun deletePage(pageId: String): Result<Unit>

    suspend fun fetchThumbnail(url: String): Result<ByteArray>

    // Owner-wide library channel: page created/deleted/updated and thumbnail
    // state. Null when there is no access token.
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

    fun shutdown()
}

// The server refused the session and refreshing the token did not fix it.
// Distinct from every other failure: only this one means "sign in again" — an
// unreachable or failing server must never sign the user out.
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
    fun onEvent(event: LibraryEvent)

    fun onError(message: String)

    fun onClosed()
}

interface PageEventListener {
    fun onEvent(event: PageEvent)

    fun onError(message: String)

    // The upgrade was refused with 401: the access token is no longer valid.
    // Told apart from [onError] because reconnecting with the same token can
    // never succeed.
    fun onUnauthorized(message: String) = onError(message)

    fun onClosed()
}
