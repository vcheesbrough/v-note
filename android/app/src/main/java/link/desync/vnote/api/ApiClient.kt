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

    // Per-page ink channel. Bidirectional: the caller subscribes for gap-fill,
    // acquires the edit lease, and commits coalesced stroke batches via the
    // returned [PageSocket]; server events arrive on [listener].
    fun openPageSocket(
        pageId: String,
        listener: PageEventListener,
    ): PageSocket?

    fun shutdown()
}

interface LibraryEventListener {
    fun onEvent(event: LibraryEvent)

    fun onError(message: String)

    fun onClosed()
}

interface PageEventListener {
    fun onEvent(event: PageEvent)

    fun onError(message: String)

    fun onClosed()
}
