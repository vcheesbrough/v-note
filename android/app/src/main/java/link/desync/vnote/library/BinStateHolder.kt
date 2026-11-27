package link.desync.vnote.library

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import link.desync.vnote.api.PageBinApi
import link.desync.vnote.model.LibraryEvent
import link.desync.vnote.model.PageSummary
import link.desync.vnote.telemetry.Telemetry

// The recycle bin's state (#631): its pages, most recently binned first, and
// whether the bin screen is showing. Owned by [LibraryStateHolder], whose
// library channel keeps the list live; the list itself is fetched each time the
// bin opens. Each action applies what its channel event would, through
// [applyEvent] — so the library and the bin follow at once, and the event, when
// it arrives, changes nothing further. Failures go to the library's banner.
internal class BinStateHolder(
    private val api: PageBinApi,
    private val applyEvent: (LibraryEvent) -> Unit,
    private val reportError: (String?) -> Unit,
) {
    var pages by mutableStateOf<List<PageSummary>>(emptyList())
        private set

    var showing by mutableStateOf(false)
        private set

    fun open() {
        showing = true
        Telemetry.startScreen(BIN_SCREEN)
        CoroutineScope(Dispatchers.Main).launch {
            api.listBin().fold(
                onSuccess = { loaded ->
                    pages = loaded
                    reportError(null)
                },
                onFailure = { failure -> reportError(failure.message ?: "Loading the bin failed") },
            )
        }
    }

    fun close() {
        showing = false
        Telemetry.startScreen(LIBRARY_SCREEN)
    }

    fun restore(page: PageSummary) {
        CoroutineScope(Dispatchers.Main).launch {
            api.restorePage(page.id).fold(
                onSuccess = { restored -> applyEvent(LibraryEvent.PageRestored(restored)) },
                onFailure = { failure -> reportError(failure.message ?: "Restoring the page failed") },
            )
        }
    }

    fun deletePermanently(page: PageSummary) {
        CoroutineScope(Dispatchers.Main).launch {
            api.deletePagePermanently(page.id).fold(
                onSuccess = { applyEvent(LibraryEvent.PageDeleted(page.id)) },
                onFailure = { failure -> reportError(failure.message ?: "Deleting the page failed") },
            )
        }
    }

    fun empty() {
        CoroutineScope(Dispatchers.Main).launch {
            val emptied = pages
            api.emptyBin().fold(
                onSuccess = { emptied.forEach { applyEvent(LibraryEvent.PageDeleted(it.id)) } },
                onFailure = { failure -> reportError(failure.message ?: "Emptying the bin failed") },
            )
        }
    }

    // One library-channel event: the bin follows it (see [binApplying]).
    fun apply(event: LibraryEvent) {
        pages = pages.binApplying(event)
    }

    // Sign-out.
    fun clear() {
        pages = emptyList()
        showing = false
    }
}

private const val BIN_SCREEN = "screen.bin"
