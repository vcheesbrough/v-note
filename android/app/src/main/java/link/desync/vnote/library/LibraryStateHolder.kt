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
import link.desync.vnote.telemetry.Telemetry
import okhttp3.WebSocket

// The signed-in page library: the page list, the open page, the library error
// banner, and the library channel that keeps them live. Extracted from
// MainActivity (#337), which keeps the sign-in flow and delegates here. The list
// rules themselves are the pure functions in LibraryPages.kt.
internal class LibraryStateHolder(
    private val apiClient: ApiClient,
    // Runs the reconnect delay and the snapshot fetch that precedes each
    // channel; the activity's lifecycle scope, which dispatches to the main
    // thread. Compose state is written on this scope, so a production scope has
    // to be main-dispatched; unit tests substitute a direct dispatcher because
    // they read the state without recomposing.
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
        // A new trace for the library (#406): one per visit, as the SPA does per
        // route, so a library left open all day does not grow one endless trace.
        Telemetry.startScreen(LIBRARY_SCREEN)
        val generation = connectionGeneration
        reconnectScope.launch { if (loadSnapshot(generation)) connect() }
    }

    fun openPage(page: PageSummary) {
        selectedPage = page
    }

    fun closePage() {
        selectedPage = null
        Telemetry.startScreen(LIBRARY_SCREEN)
        // Shares the connect path's snapshot guard: a refetch superseded by a
        // sign-out or a reconnect is dropped rather than applied late. Safe
        // because each of those either clears the list deliberately or takes its
        // own snapshot straight after.
        val generation = connectionGeneration
        CoroutineScope(Dispatchers.Main).launch { loadSnapshot(generation) }
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
            Telemetry.startScreen(LIBRARY_SCREEN)
        }
        error = null
    }

    // Fetches the list and applies it, then reports whether the caller may go on
    // to open a channel. State is written on the caller's dispatcher, which is
    // the main thread at every call site (see [reconnectScope]).
    //
    // Callers that connect must await this rather than fire it off alongside
    // `connect()`: the assignment below replaces the whole list, so a fetch
    // resolving *after* a channel event silently rolls that event back, leaving
    // a stale library until the next refetch (#347). With the channel opened
    // only afterwards, no event can be lost that way. A failed fetch still
    // connects — the banner reports it, and the channel is what recovers.
    //
    // The cost is that realtime waits on this call: `OkHttpClient()`'s default
    // 10 s read timeout with no `callTimeout`, and a 401 here adds a refresh and
    // a second request, so the worst case is roughly two request cycles before
    // any event can arrive — on top of the reconnect backoff. Worth it, because
    // a dropped event is silent and permanent where a late channel is neither.
    // The option not taken: connect first and buffer events until the snapshot
    // lands, which would also close the gap between the two.
    //
    // The fetch is a full round trip, so [generation] is re-checked across it. A
    // sign-out or a newer channel started while it was in flight must win: this
    // snapshot is stale by then, and must neither repopulate a signed-out
    // library nor open a stray socket.
    private suspend fun loadSnapshot(generation: Int): Boolean {
        val snapshot = apiClient.listPages()
        if (!channelMayReconnect(generation, connectionGeneration, isSignedIn())) return false
        snapshot.fold(
            onSuccess = { loaded ->
                pages = loaded
                error = null
            },
            onFailure = { failure ->
                error = failure.message ?: "Loading pages failed"
            },
        )
        return true
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
                            if (loadSnapshot(generation)) connect()
                        }
                    }
                },
            )
    }
}

private const val LIBRARY_SCREEN = "screen.library"

// Whether a library channel opened as [generation] may still reconnect: it must
// be the newest channel ([currentGeneration]), and the session must still be
// signed in. Top-level so it is tested directly (LibraryStateHolderTest).
internal fun channelMayReconnect(
    generation: Int,
    currentGeneration: Int,
    signedIn: Boolean,
): Boolean = generation == currentGeneration && signedIn
