package link.desync.vnote.library

import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import link.desync.vnote.api.ApiClient
import link.desync.vnote.api.LibraryEventListener
import link.desync.vnote.api.PageEventListener
import link.desync.vnote.api.PageSocket
import link.desync.vnote.ink.Paper
import link.desync.vnote.model.LibraryEvent
import link.desync.vnote.model.MeProfile
import link.desync.vnote.model.PageSummary
import okhttp3.Request
import okhttp3.WebSocket
import okio.ByteString
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The library holder's own decisions, beyond the list rules in
 * [LibraryPagesTest]: closing a deleted open page, refusing to reconnect a
 * stale or signed-out channel (#337 review), and fetching the snapshot only
 * once the channel is open (#635) without letting it roll back an event
 * (#347).
 */
class LibraryStateHolderTest {
    private val holder =
        LibraryStateHolder(
            apiClient = UnusedApiClient,
            reconnectScope = CoroutineScope(Job()),
            isSignedIn = { true },
            runOnUiThread = { block -> block() },
        )

    @Test
    fun deletingTheOpenPageClosesIt() {
        holder.applyLibraryEvent(LibraryEvent.PageCreated(page("a")))
        holder.openPage(page("a"))

        holder.applyLibraryEvent(LibraryEvent.PageDeleted("a"))

        assertNull(holder.selectedPage)
        assertTrue(holder.pages.isEmpty())
    }

    // #631: a page binned while it is open stays open — its ink is kept and the
    // page channel's notice offers Restore — while it moves to the bin.
    @Test
    fun binningTheOpenPageKeepsItOpenAndFilesItInTheBin() {
        holder.applyLibraryEvent(LibraryEvent.PageCreated(page("a")))
        holder.openPage(page("a"))

        val binned = page("a").copy(binnedAt = "2026-11-27T21:00:00Z")
        holder.applyLibraryEvent(LibraryEvent.PageBinned(binned))

        assertEquals("a", holder.selectedPage?.id)
        assertTrue(holder.pages.isEmpty())
        assertEquals(listOf(binned), holder.bin.pages)

        holder.applyLibraryEvent(LibraryEvent.PageRestored(page("a")))
        assertEquals(listOf("a"), holder.pages.map { it.id })
        assertTrue(holder.bin.pages.isEmpty())
    }

    @Test
    fun deletingAnotherPageLeavesTheOpenPageOpen() {
        holder.applyLibraryEvent(LibraryEvent.PageCreated(page("a")))
        holder.applyLibraryEvent(LibraryEvent.PageCreated(page("b")))
        holder.openPage(page("a"))

        holder.applyLibraryEvent(LibraryEvent.PageDeleted("b"))

        assertEquals(page("a"), holder.selectedPage)
        assertEquals(listOf("a"), holder.pages.map { it.id })
    }

    @Test
    fun onlyTheNewestChannelMayReconnectAndOnlyWhileSignedIn() {
        assertTrue(channelMayReconnect(generation = 1, currentGeneration = 1, signedIn = true))
        assertFalse("a signed-out channel must not reconnect", channelMayReconnect(1, 1, signedIn = false))
        assertFalse("a superseded channel must not reconnect", channelMayReconnect(1, 2, signedIn = true))
    }

    @Test
    fun signingOutSupersedesEveryOpenChannel() {
        val before = holder.connectionGeneration

        holder.clear()

        assertEquals(before + 1, holder.connectionGeneration)
        assertFalse(channelMayReconnect(before, holder.connectionGeneration, signedIn = true))
    }

    /**
     * #635: the snapshot is read only once the channel is open. The server
     * subscribes a socket before it answers the upgrade (#631), so from
     * `onOpen` on every event is queued on it; fetching first left a window
     * in which an event published elsewhere was lost until the next refetch.
     */
    @Test
    fun theSnapshotIsFetchedOnlyOnceTheChannelHasOpened() {
        val api = ChannelControlledApiClient()
        val holder = holderWith(api)

        holder.start()

        assertEquals("the channel opens first", 1, api.socketsOpened)
        assertEquals("no snapshot before the channel is open", 0, api.listPagesCalls)

        api.openChannel()
        assertEquals("the snapshot follows the open", 1, api.listPagesCalls)

        api.completeListPages(Result.success(listOf(page("new"), page("old"))))
        assertEquals(listOf("new", "old"), holder.pages.map { it.id })
    }

    /**
     * The #347 guarantee, kept: the snapshot assigns the whole list, so an
     * event that arrives while it is in flight must not be rolled back when
     * it lands. Here a page created elsewhere arrives mid-fetch, and the
     * snapshot (read before it) does not have it.
     */
    @Test
    fun anEventDuringTheSnapshotIsNotRolledBackByIt() {
        val api = ChannelControlledApiClient()
        val holder = holderWith(api)
        holder.start()
        api.openChannel()

        api.sendEvent(LibraryEvent.PageCreated(page("live", updatedAt = "2026-07-23T00:00:00Z")))
        api.completeListPages(Result.success(listOf(page("old"))))

        assertEquals(listOf("live", "old"), holder.pages.map { it.id })
    }

    /** A removal mid-fetch survives a snapshot that still lists the page. */
    @Test
    fun aPageBinnedDuringTheSnapshotStaysOutOfTheLibrary() {
        val api = ChannelControlledApiClient()
        val holder = holderWith(api)
        holder.start()
        api.openChannel()

        api.sendEvent(LibraryEvent.PageBinned(page("gone").copy(binnedAt = "2026-07-23T00:00:00Z")))
        api.completeListPages(Result.success(listOf(page("gone"), page("kept"))))

        assertEquals(listOf("kept"), holder.pages.map { it.id })
    }

    /**
     * Replaying an event the snapshot already reflects changes nothing: the
     * snapshot was read after the page was edited again, and the replayed
     * creation must not undo that edit.
     */
    @Test
    fun replayingAnEventTheSnapshotAlreadyHasIsHarmless() {
        val api = ChannelControlledApiClient()
        val holder = holderWith(api)
        holder.start()
        api.openChannel()

        api.sendEvent(LibraryEvent.PageCreated(page("a", updatedAt = "2026-07-23T00:00:00Z")))
        api.sendEvent(LibraryEvent.PageUpdated("a", "2026-07-24T00:00:00Z"))
        api.completeListPages(Result.success(listOf(page("a", updatedAt = "2026-07-24T00:00:00Z"), page("b"))))

        assertEquals(listOf("a", "b"), holder.pages.map { it.id })
        assertEquals("2026-07-24T00:00:00Z", holder.pages.first().updatedAt)
    }

    /** A failed snapshot leaves the live channel and its events in place. */
    @Test
    fun aFailedSnapshotKeepsTheChannelAndItsEvents() {
        val api = ChannelControlledApiClient()
        val holder = holderWith(api)
        holder.start()
        api.openChannel()

        api.sendEvent(LibraryEvent.PageCreated(page("live")))
        api.completeListPages(Result.failure(RuntimeException("offline")))

        assertEquals("offline", holder.error)
        assertEquals(listOf("live"), holder.pages.map { it.id })
        assertEquals("no reconnect: the channel is still live", 1, api.socketsOpened)
    }

    /**
     * A channel refused before it opens took no snapshot. The list still
     * loads — its REST call is also what refreshes an expired token, which
     * the upgrade cannot — and the realtime banner stays up, because the
     * library is not live.
     */
    @Test
    fun aChannelThatNeverOpensStillLoadsTheListAndKeepsTheBanner() {
        val api = ChannelControlledApiClient()
        val holder = holderWith(api)
        holder.start()

        api.failChannel("Realtime connection failed: HTTP 503")
        assertEquals(1, api.listPagesCalls)
        api.completeListPages(Result.success(listOf(page("a"))))

        assertEquals(listOf("a"), holder.pages.map { it.id })
        assertEquals("Realtime connection failed: HTTP 503", holder.error)
    }

    /**
     * The sign-out guard holds across the fetch: a snapshot that lands after
     * sign-out neither repopulates the library, and nor does a late event
     * from the channel sign-out closed.
     */
    @Test
    fun signingOutWhileTheSnapshotIsInFlightNeitherRepopulatesNorApplies() {
        val api = ChannelControlledApiClient()
        val holder = holderWith(api)

        holder.start()
        api.openChannel()
        holder.clear()
        api.completeListPages(Result.success(listOf(page("new"))))
        api.sendEvent(LibraryEvent.PageCreated(page("late")))

        assertTrue("a superseded snapshot must not repopulate the library", holder.pages.isEmpty())
        assertEquals("no reconnect after sign-out", 1, api.socketsOpened)
    }

    // Unconfined runs each coroutine inline on this thread up to its first
    // suspension and resumes it inline on the thread that completes it, so the
    // tests above step the fetch by hand with no sleeps or idling. Cancelled
    // after each test, so a reconnect's delay never fires into the next one.
    private val scopeJob = Job()

    @After
    fun cancelReconnects() {
        scopeJob.cancel()
    }

    private fun holderWith(api: ApiClient): LibraryStateHolder =
        LibraryStateHolder(
            apiClient = api,
            reconnectScope = CoroutineScope(Dispatchers.Unconfined + scopeJob),
            isSignedIn = { true },
            runOnUiThread = { block -> block() },
        )

    private fun page(
        id: String,
        updatedAt: String = "2026-07-22T00:00:00Z",
    ): PageSummary = PageSummary(id = id, title = id, createdAt = "2026-07-22T00:00:00Z", updatedAt = updatedAt)

    // Holds each `listPages` open until the test releases it, and drives the
    // newest channel's listener by hand, so the order of channel, snapshot and
    // events is observable.
    private class ChannelControlledApiClient : ApiClient by UnusedApiClient {
        private val pending = ArrayDeque<CompletableDeferred<Result<List<PageSummary>>>>()
        private var listener: LibraryEventListener? = null

        var socketsOpened = 0
            private set

        var listPagesCalls = 0
            private set

        fun openChannel() = checkNotNull(listener).onOpen()

        fun sendEvent(event: LibraryEvent) = checkNotNull(listener).onEvent(event)

        fun failChannel(message: String) = checkNotNull(listener).onError(message)

        fun completeListPages(result: Result<List<PageSummary>>) {
            pending.removeFirst().complete(result)
        }

        override suspend fun listPages(): Result<List<PageSummary>> {
            listPagesCalls += 1
            val reply = CompletableDeferred<Result<List<PageSummary>>>()
            pending.addLast(reply)
            return reply.await()
        }

        // A real socket is not needed: the holder only closes it.
        override fun openLibrarySocket(listener: LibraryEventListener): WebSocket {
            socketsOpened += 1
            this.listener = listener
            return ClosableSocket
        }
    }

    private object ClosableSocket : WebSocket {
        override fun request(): Request = error("unused")

        override fun queueSize(): Long = 0

        override fun send(text: String): Boolean = false

        override fun send(bytes: ByteString): Boolean = false

        override fun close(
            code: Int,
            reason: String?,
        ): Boolean = true

        override fun cancel() = Unit
    }

    // The baseline every fake narrows: a call nobody overrode is a test bug.
    private object UnusedApiClient : ApiClient {
        override val baseUrl: String get() = error("unused")

        override suspend fun checkHealth(): Result<Unit> = error("unused")

        override suspend fun fetchMe(): Result<MeProfile> = error("unused")

        override suspend fun listPages(): Result<List<PageSummary>> = error("unused")

        override suspend fun createPage(
            title: String?,
            paper: Paper,
        ): Result<PageSummary> = error("unused")

        override suspend fun deletePage(pageId: String): Result<Unit> = error("unused")

        override suspend fun listBin(): Result<List<PageSummary>> = error("unused")

        override suspend fun restorePage(pageId: String): Result<PageSummary> = error("unused")

        override suspend fun deletePagePermanently(pageId: String): Result<Unit> = error("unused")

        override suspend fun emptyBin(pageIds: List<String>): Result<List<String>> = error("unused")

        override suspend fun fetchThumbnail(url: String): Result<ByteArray> = error("unused")

        override fun openLibrarySocket(listener: LibraryEventListener): WebSocket? = error("unused")

        override fun openPageSocket(
            pageId: String,
            listener: PageEventListener,
        ): PageSocket? = error("unused")

        override fun shutdown() = error("unused")
    }
}
