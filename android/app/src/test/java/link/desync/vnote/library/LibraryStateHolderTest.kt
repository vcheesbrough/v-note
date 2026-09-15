package link.desync.vnote.library

import kotlinx.coroutines.CoroutineScope
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
 * [LibraryPagesTest]: closing a deleted open page, and refusing to reconnect a
 * stale or signed-out channel (#337 review).
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

    private fun page(id: String): PageSummary =
        PageSummary(id = id, title = id, createdAt = "2026-07-22T00:00:00Z", updatedAt = "2026-07-22T00:00:00Z")

    // Nothing under test reaches the network; any call is a test bug.
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
