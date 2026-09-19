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
import okhttp3.WebSocket
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The library holder's own decisions, beyond the list rules in
 * [LibraryPagesTest]: closing a deleted open page, refusing to reconnect a
 * stale or signed-out channel (#337 review), and ordering the snapshot fetch
 * ahead of the channel it precedes (#347).
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
     * The regression behind #347: the snapshot fetch assigns the whole list, so
     * while it is in flight the channel must not be live — an event delivered
     * in that window was silently rolled back when the fetch landed on top of
     * it, leaving a stale library until the next refetch.
     */
    @Test
    fun theChannelOpensOnlyOnceTheSnapshotHasBeenApplied() {
        val api = SnapshotControlledApiClient()
        val holder = holderWith(api)

        holder.start()

        assertEquals("no channel while the snapshot is in flight", 0, api.socketsOpened)
        assertTrue(holder.pages.isEmpty())

        api.completeListPages(Result.success(listOf(page("new"), page("old"))))

        assertEquals(listOf("new", "old"), holder.pages.map { it.id })
        assertEquals("the channel opens once the list is applied", 1, api.socketsOpened)
    }

    /** A library that failed to load still needs the channel to recover it. */
    @Test
    fun aFailedSnapshotStillOpensTheChannel() {
        val api = SnapshotControlledApiClient()
        val holder = holderWith(api)

        holder.start()
        api.completeListPages(Result.failure(RuntimeException("offline")))

        assertEquals("offline", holder.error)
        assertTrue("a failed snapshot must not touch the list", holder.pages.isEmpty())
        assertEquals(1, api.socketsOpened)
    }

    /**
     * Awaiting the snapshot widens the window between the decision to connect
     * and the connect itself, so the sign-out guard has to hold across it.
     */
    @Test
    fun signingOutWhileTheSnapshotIsInFlightNeitherConnectsNorRepopulates() {
        val api = SnapshotControlledApiClient()
        val holder = holderWith(api)

        holder.start()
        holder.clear()
        api.completeListPages(Result.success(listOf(page("new"))))

        assertEquals("a superseded snapshot must not open a channel", 0, api.socketsOpened)
        assertTrue("a superseded snapshot must not repopulate the library", holder.pages.isEmpty())
    }

    // Unconfined runs each coroutine inline on this thread up to its first
    // suspension and resumes it inline on the thread that completes it, so the
    // tests above step the fetch by hand with no sleeps or idling.
    private fun holderWith(api: ApiClient): LibraryStateHolder =
        LibraryStateHolder(
            apiClient = api,
            reconnectScope = CoroutineScope(Dispatchers.Unconfined),
            isSignedIn = { true },
            runOnUiThread = { block -> block() },
        )

    private fun page(id: String): PageSummary =
        PageSummary(id = id, title = id, createdAt = "2026-07-22T00:00:00Z", updatedAt = "2026-07-22T00:00:00Z")

    // Holds `listPages` open until the test releases it, and counts the channels
    // opened, so the order of snapshot and channel is observable.
    private class SnapshotControlledApiClient : ApiClient by UnusedApiClient {
        private val pending = CompletableDeferred<Result<List<PageSummary>>>()

        var socketsOpened = 0
            private set

        fun completeListPages(result: Result<List<PageSummary>>) {
            pending.complete(result)
        }

        override suspend fun listPages(): Result<List<PageSummary>> = pending.await()

        override fun openLibrarySocket(listener: LibraryEventListener): WebSocket? {
            socketsOpened += 1
            return null
        }
    }

    // The baseline every fake narrows: a call nobody overrode is a test bug.
    private object UnusedApiClient : ApiClient {
        override suspend fun fetchMe(): Result<MeProfile> = error("unused")

        override suspend fun listPages(): Result<List<PageSummary>> = error("unused")

        override suspend fun createPage(
            title: String?,
            paper: Paper,
        ): Result<PageSummary> = error("unused")

        override suspend fun deletePage(pageId: String): Result<Unit> = error("unused")

        override suspend fun fetchThumbnail(url: String): Result<ByteArray> = error("unused")

        override fun openLibrarySocket(listener: LibraryEventListener): WebSocket? = error("unused")

        override fun openPageSocket(
            pageId: String,
            listener: PageEventListener,
        ): PageSocket? = error("unused")

        override fun shutdown() = error("unused")
    }
}
