package link.desync.vnote

import link.desync.vnote.api.ApiClient
import link.desync.vnote.api.ConnectionFailure
import link.desync.vnote.api.LibraryEventListener
import link.desync.vnote.api.PageEventListener
import link.desync.vnote.api.PageSocket
import link.desync.vnote.api.ServerUnreachableException
import link.desync.vnote.api.hostOf
import link.desync.vnote.auth.RefreshOutcome
import link.desync.vnote.ink.Paper
import link.desync.vnote.model.MeProfile
import link.desync.vnote.model.PageSummary
import link.desync.vnote.sync.ErrorClass
import okhttp3.Request
import okhttp3.WebSocket
import okio.ByteString
import java.net.UnknownHostException

/**
 * The app's one [ApiClient], with the server behind it swappable mid-test
 * (#651): [current] is a real client, and pointing it at a different address
 * — one that speaks no TLS, one that never answers — is a real transport
 * failure of that kind, on a device that may only ever talk to loopback.
 *
 * The one failure loopback cannot produce is a name that does not resolve
 * (the instrumentation's network guard refuses every non-loopback host before
 * DNS, #464). [dnsDown] stands in for it at this boundary, with exactly what
 * the real client hands the app for an `UnknownHostException`.
 */
internal class SwitchableApiClient(
    @Volatile var current: ApiClient,
) : ApiClient {
    @Volatile
    var dnsDown = false

    // How many page channels [dnsDown] has failed.
    @Volatile
    var pageSocketsRefused = 0
        private set

    override val baseUrl: String get() = current.baseUrl
    override val signInHost: String get() = current.signInHost
    override val lastRefreshOutcome: RefreshOutcome get() = current.lastRefreshOutcome

    override suspend fun checkHealth(): Result<Unit> = resolving { current.checkHealth() }

    override suspend fun fetchMe(): Result<MeProfile> = resolving { current.fetchMe() }

    override suspend fun listPages(): Result<List<PageSummary>> = resolving { current.listPages() }

    override suspend fun createPage(
        title: String?,
        paper: Paper,
        id: String?,
    ): Result<PageSummary> = resolving { current.createPage(title, paper, id) }

    override suspend fun deletePage(pageId: String): Result<Unit> = resolving { current.deletePage(pageId) }

    override suspend fun listBin(): Result<List<PageSummary>> = resolving { current.listBin() }

    override suspend fun restorePage(pageId: String): Result<PageSummary> = resolving { current.restorePage(pageId) }

    override suspend fun deletePagePermanently(pageId: String): Result<Unit> = resolving { current.deletePagePermanently(pageId) }

    override suspend fun emptyBin(pageIds: List<String>): Result<List<String>> = resolving { current.emptyBin(pageIds) }

    override suspend fun fetchThumbnail(url: String): Result<ByteArray> = resolving { current.fetchThumbnail(url) }

    override fun openLibrarySocket(listener: LibraryEventListener): WebSocket? {
        if (!dnsDown) return current.openLibrarySocket(listener)
        listener.onConnectionFailure(unresolved())
        return DeadSocket
    }

    override fun openPageSocket(
        pageId: String,
        listener: PageEventListener,
    ): PageSocket? {
        if (!dnsDown) return current.openPageSocket(pageId, listener)
        pageSocketsRefused += 1
        listener.onConnectionFailure(unresolved())
        return PageSocket(DeadSocket)
    }

    override suspend fun refreshSession(): Boolean = current.refreshSession()

    override fun shutdown() = current.shutdown()

    private inline fun <T> resolving(call: () -> Result<T>): Result<T> =
        if (dnsDown) Result.failure(ServerUnreachableException(unknownHost())) else call()

    private fun unresolved() =
        ConnectionFailure(
            message = ServerUnreachableException(unknownHost()).message.orEmpty(),
            host = hostOf(baseUrl),
            errorClass = ErrorClass.OFFLINE,
            correlationId = null,
        )

    private fun unknownHost() = UnknownHostException("Unable to resolve host \"${hostOf(baseUrl)}\": No address associated with hostname")

    // A socket that never connected: nothing to send on, nothing to close.
    private object DeadSocket : WebSocket {
        override fun request(): Request = error("never connected")

        override fun queueSize(): Long = 0

        override fun send(text: String): Boolean = false

        override fun send(bytes: ByteString): Boolean = false

        override fun close(
            code: Int,
            reason: String?,
        ): Boolean = false

        override fun cancel() = Unit
    }
}
