package link.desync.vnote.library

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import link.desync.vnote.api.ApiClient
import link.desync.vnote.api.LibraryEventListener
import link.desync.vnote.ink.Paper
import link.desync.vnote.model.LibraryEvent
import link.desync.vnote.model.PageSummary
import okhttp3.WebSocket

// The signed-in page library: the page list, the open page, the library error
// banner, and the library channel that keeps them live. Extracted from
// MainActivity (#337), which keeps the sign-in flow and delegates here. The list
// rules themselves are the pure functions in LibraryPages.kt.
internal class LibraryStateHolder(
    private val apiClient: ApiClient,
    // Runs the reconnect delay; the activity's lifecycle scope.
    private val reconnectScope: CoroutineScope,
    // A channel that closes after sign-out must not reconnect.
    private val isSignedIn: () -> Boolean,
    // Channel callbacks arrive on OkHttp threads; state is written on the UI thread.
    private val runOnUiThread: (() -> Unit) -> Unit,
) {
    var pages by mutableStateOf<List<PageSummary>>(emptyList())
        private set

    var selectedPage by mutableStateOf<PageSummary?>(null)
        private set

    var error by mutableStateOf<String?>(null)
        private set

    private var librarySocket: WebSocket? = null

    // Bumped for every new channel and on sign-out; a channel remembers the value
    // it was opened with. Readable so tests can observe sign-out superseding it.
    internal var connectionGeneration = 0
        private set

    // Loads the list and opens the library channel, once a session is signed in.
    fun start() {
        loadPages()
        connect()
    }

    fun openPage(page: PageSummary) {
        selectedPage = page
    }

    fun closePage() {
        selectedPage = null
        loadPages()
    }

    fun createPage(paper: Paper) {
        CoroutineScope(Dispatchers.Main).launch {
            apiClient.createPage(paper = paper).fold(
                onSuccess = { page ->
                    pages = pages.upsert(page)
                    selectedPage = page
                    error = null
                },
                onFailure = { failure ->
                    error = failure.message ?: "Creating page failed"
                },
            )
        }
    }

    fun deletePage(page: PageSummary) {
        CoroutineScope(Dispatchers.Main).launch {
            apiClient.deletePage(page.id).fold(
                // Exactly what the channel's own page-deleted event does: drop
                // it, close it if open, clear the error.
                onSuccess = { applyLibraryEvent(LibraryEvent.PageDeleted(page.id)) },
                onFailure = { failure ->
                    error = failure.message ?: "Deleting page failed"
                },
            )
        }
    }

    // Sign-out: close the channel for good, and forget the list and the open page.
    fun clear() {
        librarySocket?.close(1000, "signed out")
        librarySocket = null
        connectionGeneration += 1
        pages = emptyList()
        selectedPage = null
    }

    // Activity teardown.
    fun close() {
        librarySocket?.close(1000, "activity destroyed")
    }

    // One library-channel event, on the UI thread: the list follows the event, a
    // deleted open page is closed, and a live event clears the error banner.
    internal fun applyLibraryEvent(event: LibraryEvent) {
        pages = pages.applying(event)
        if (event is LibraryEvent.PageDeleted && selectedPage?.id == event.pageId) {
            selectedPage = null
        }
        error = null
    }

    // The REST calls here run in a fresh Main-dispatcher scope per call, not a
    // lifecycle scope, exactly as they did inside MainActivity.
    private fun loadPages() {
        CoroutineScope(Dispatchers.Main).launch {
            apiClient.listPages().fold(
                onSuccess = { loaded ->
                    pages = loaded
                    error = null
                },
                onFailure = { failure ->
                    error = failure.message ?: "Loading pages failed"
                },
            )
        }
    }

    private fun connect() {
        val generation = ++connectionGeneration
        librarySocket?.close(1000, "reconnecting")
        librarySocket =
            apiClient.openLibrarySocket(
                object : LibraryEventListener {
                    override fun onEvent(event: LibraryEvent) {
                        runOnUiThread { applyLibraryEvent(event) }
                    }

                    override fun onError(message: String) {
                        runOnUiThread { error = message }
                    }

                    override fun onClosed() {
                        if (!channelMayReconnect(generation, connectionGeneration, isSignedIn())) return
                        runOnUiThread { error = "Realtime disconnected" }
                        reconnectScope.launch {
                            delay(1_000)
                            if (!channelMayReconnect(generation, connectionGeneration, isSignedIn())) return@launch
                            loadPages()
                            connect()
                        }
                    }
                },
            )
    }
}

// Whether a library channel opened as [generation] may still reconnect: it must
// be the newest channel ([currentGeneration]), and the session must still be
// signed in. Top-level so it is tested directly (LibraryStateHolderTest).
internal fun channelMayReconnect(
    generation: Int,
    currentGeneration: Int,
    signedIn: Boolean,
): Boolean = generation == currentGeneration && signedIn
