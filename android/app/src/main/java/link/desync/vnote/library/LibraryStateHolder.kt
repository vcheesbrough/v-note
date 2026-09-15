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
class LibraryStateHolder(
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
    private var connectionGeneration = 0

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

    // The REST calls below run in a fresh Main-dispatcher scope per call, not a
    // lifecycle scope, exactly as they did inside MainActivity.
    fun loadPages() {
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
                onSuccess = {
                    removePage(page.id)
                    error = null
                },
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

    private fun connect() {
        val generation = ++connectionGeneration
        librarySocket?.close(1000, "reconnecting")
        librarySocket =
            apiClient.openLibrarySocket(
                object : LibraryEventListener {
                    override fun onEvent(event: LibraryEvent) {
                        runOnUiThread {
                            pages = pages.applying(event)
                            if (event is LibraryEvent.PageDeleted && selectedPage?.id == event.pageId) {
                                selectedPage = null
                            }
                            error = null
                        }
                    }

                    override fun onError(message: String) {
                        runOnUiThread { error = message }
                    }

                    override fun onClosed() {
                        if (generation != connectionGeneration || !isSignedIn()) return
                        runOnUiThread { error = "Realtime disconnected" }
                        reconnectScope.launch {
                            delay(1_000)
                            if (generation != connectionGeneration || !isSignedIn()) return@launch
                            loadPages()
                            connect()
                        }
                    }
                },
            )
    }

    private fun removePage(pageId: String) {
        pages = pages.without(pageId)
        if (selectedPage?.id == pageId) {
            selectedPage = null
        }
    }
}
