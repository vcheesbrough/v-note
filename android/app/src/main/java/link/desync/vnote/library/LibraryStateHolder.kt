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
import java.util.concurrent.atomic.AtomicBoolean

// The signed-in page library: the page list, the open page, the library error
// banner, and the library channel that keeps them live. Extracted from
// MainActivity (#337), which keeps the sign-in flow and delegates here. The list
// rules themselves are the pure functions in LibraryPages.kt.
internal class LibraryStateHolder(
    private val apiClient: ApiClient,
    // Runs the reconnect delay and the snapshot fetch that follows each
    // channel's open; the activity's lifecycle scope, which dispatches to the main
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

    // The bin screen (#631), kept live by this holder's library channel.
    val bin = BinStateHolder(apiClient, applyEvent = { applyLibraryEvent(it) }, reportError = { error = it })

    var error by mutableStateOf<String?>(null)
        private set

    private var librarySocket: WebSocket? = null

    // One per snapshot fetch in flight: the events applied while it is out,
    // replayed on top of the list it brings back (see [loadSnapshot]). Only
    // touched on the UI thread.
    private val snapshotRecorders = mutableListOf<EventRecorder>()

    // Bumped for every new channel and on sign-out; a channel remembers the value
    // it was opened with. Readable so tests can observe sign-out superseding it.
    internal var connectionGeneration = 0
        private set

    // Loads the list and opens the library channel, once a session is signed in.
    fun start() {
        // A new trace for the library (#406): one per visit, as the SPA does per
        // route, so a library left open all day does not grow one endless trace.
        Telemetry.startScreen(LIBRARY_SCREEN)
        connect()
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
        // own snapshot once its channel opens. Events that arrive on the live
        // channel meanwhile are replayed on top, so this refetch cannot roll
        // one back either.
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

    // Moves the page to the bin (#631). The page leaves the library at once;
    // the channel's `page-binned` files it in the bin with its time.
    fun deletePage(page: PageSummary) {
        CoroutineScope(Dispatchers.Main).launch {
            apiClient.deletePage(page.id).fold(
                onSuccess = {
                    pages = pages.without(page.id)
                    error = null
                },
                onFailure = { failure ->
                    error = failure.message ?: "Moving the page to the bin failed"
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
        bin.clear()
    }

    // Activity teardown.
    fun close() {
        librarySocket?.close(1000, "activity destroyed")
    }

    // One library-channel event, on the UI thread: both lists follow the event,
    // a permanently deleted open page is closed, and a live event clears the
    // error banner. A *binned* open page stays open (#631): its ink is kept and
    // the page channel's notice offers to restore it.
    internal fun applyLibraryEvent(event: LibraryEvent) {
        snapshotRecorders.forEach { it.events += event }
        pages = pages.applying(event)
        bin.apply(event)
        if (event is LibraryEvent.PageDeleted && selectedPage?.id == event.pageId) {
            selectedPage = null
            Telemetry.startScreen(LIBRARY_SCREEN)
        }
        error = null
    }

    // Fetches the list and applies it, then reports whether the snapshot was
    // still current (see below). State is written on the caller's dispatcher,
    // which is the main thread at every call site (see [reconnectScope]).
    //
    // The channel opens *first* and this runs from its `onOpen` (#635). The
    // server subscribes a library socket before it answers the upgrade (#631),
    // so once the socket is open every later event is queued on it; a snapshot
    // read after that cannot miss one. Loading first and connecting after, as
    // #347 did, lost any event published between the snapshot and the
    // subscription (a page created, binned, restored or deleted elsewhere)
    // until the next refetch.
    //
    // The assignment below replaces the whole list, so an event applied while
    // the fetch is in flight would be rolled back when it lands (#347). Each
    // fetch therefore records the events applied meanwhile and replays them,
    // in order, on top of what it brought back. Replaying one the snapshot
    // already reflects is harmless: the reducers are upserts, removals and
    // newer-only updates (LibraryPages.kt), so the result is the snapshot
    // plus everything that happened after it.
    //
    // The fetch is a full round trip, so [generation] is re-checked across it. A
    // sign-out or a newer channel started while it was in flight must win: this
    // snapshot is stale by then, and must neither repopulate a signed-out
    // library nor let a superseded channel reconnect. [clearsError] is false
    // when the channel is down, so a list that loaded does not hide the banner
    // saying realtime is not live.
    private suspend fun loadSnapshot(
        generation: Int,
        clearsError: Boolean = true,
    ): Boolean {
        val recorder = EventRecorder()
        snapshotRecorders += recorder
        val snapshot =
            try {
                apiClient.listPages()
            } finally {
                snapshotRecorders -= recorder
            }
        if (!channelMayReconnect(generation, connectionGeneration, isSignedIn())) return false
        snapshot.fold(
            onSuccess = { loaded ->
                pages = recorder.events.fold(loaded) { list, event -> list.applying(event) }
                if (clearsError) error = null
            },
            onFailure = { failure ->
                error = failure.message ?: "Loading pages failed"
            },
        )
        return true
    }

    private fun connect() {
        val generation = ++connectionGeneration
        // Set on OkHttp's thread before any event or failure of this socket,
        // which OkHttp reports on that same thread, in order.
        val opened = AtomicBoolean(false)
        // A failure — no network, refused, reset — ends the socket as surely
        // as a close does, and OkHttp reports only one of the two. Both
        // reconnect (#150): reconnecting only on a clean close left the
        // library stale for good after airplane mode.
        val reconnectAfter = { message: String ->
            if (channelMayReconnect(generation, connectionGeneration, isSignedIn())) {
                runOnUiThread { error = message }
                reconnectScope.launch {
                    // A channel that never opened took no snapshot. Take one
                    // anyway: the list still loads when only realtime is
                    // refused, and its REST call refreshes an expired token on
                    // a 401, which an upgrade cannot do for itself.
                    val superseded = !opened.get() && !loadSnapshot(generation, clearsError = false)
                    if (!superseded) {
                        delay(1_000)
                        if (channelMayReconnect(generation, connectionGeneration, isSignedIn())) connect()
                    }
                }
            }
        }
        librarySocket?.close(1000, "reconnecting")
        librarySocket =
            apiClient.openLibrarySocket(
                object : LibraryEventListener {
                    override fun onOpen() {
                        opened.set(true)
                        reconnectScope.launch { loadSnapshot(generation) }
                    }

                    // A superseded or signed-out channel's late event is
                    // dropped: the newer channel's snapshot covers it.
                    override fun onEvent(event: LibraryEvent) {
                        runOnUiThread {
                            if (generation == connectionGeneration) applyLibraryEvent(event)
                        }
                    }

                    override fun onError(message: String) {
                        reconnectAfter(message)
                    }

                    override fun onClosed() {
                        reconnectAfter("Realtime disconnected")
                    }
                },
            )
        // No access token, so no channel: the list is all there is to load.
        if (librarySocket == null) reconnectScope.launch { loadSnapshot(generation) }
    }

    // Identity, not contents: two fetches in flight with nothing recorded yet
    // must not be mistaken for each other when one of them finishes.
    private class EventRecorder {
        val events = mutableListOf<LibraryEvent>()
    }
}

internal const val LIBRARY_SCREEN = "screen.library"

// Whether a library channel opened as [generation] may still reconnect: it must
// be the newest channel ([currentGeneration]), and the session must still be
// signed in. Top-level so it is tested directly (LibraryStateHolderTest).
internal fun channelMayReconnect(
    generation: Int,
    currentGeneration: Int,
    signedIn: Boolean,
): Boolean = generation == currentGeneration && signedIn
