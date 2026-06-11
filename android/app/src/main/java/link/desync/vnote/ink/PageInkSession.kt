package link.desync.vnote.ink

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.launch
import link.desync.vnote.auth.ApiClient
import link.desync.vnote.auth.PageEvent
import link.desync.vnote.auth.PageEventListener
import link.desync.vnote.auth.PageSocket
import link.desync.vnote.auth.Stroke
import java.util.UUID

// Drives one open page's ink channel: connects the WSS, replays persisted
// strokes (gap-fill), tracks the single-editor edit lease, and commits new
// strokes. Exposes Compose state for the canvas. Always-online MVP — a full
// disconnect/error UX lands in a later card.
class PageInkSession(
    private val apiClient: ApiClient,
    private val pageId: String,
    private val scope: CoroutineScope,
) {
    // Committed strokes in commit order; drives the canvas render.
    val strokes = mutableStateListOf<Stroke>()

    // True only while this session holds the edit lease.
    var canEdit by mutableStateOf(false)
        private set

    // Non-null when there is something to surface to the user (blocked/error).
    var statusBanner by mutableStateOf<String?>(null)
        private set

    private var sessionId: String? = null
    private var lastSeq: Long = 0
    private val seenBatchIds = mutableSetOf<String>()
    private var socket: PageSocket? = null

    fun connect() {
        statusBanner = CONNECTING
        val opened =
            apiClient.openPageSocket(
                pageId,
                object : PageEventListener {
                    override fun onEvent(event: PageEvent) {
                        scope.launch { handle(event) }
                    }

                    override fun onError(message: String) {
                        scope.launch { statusBanner = message }
                    }

                    override fun onClosed() {
                        scope.launch { statusBanner = DISCONNECTED }
                    }
                },
            )
        socket = opened
        if (opened == null) {
            statusBanner = NOT_SIGNED_IN
        }
    }

    fun disconnect() {
        socket?.releaseLease()
        socket?.close()
        socket = null
    }

    // Commit a freshly captured stroke. Optimistically rendered locally, then
    // echoed back by the server (deduped by client batch id) with its seq.
    fun commitStroke(stroke: Stroke) {
        if (!canEdit || stroke.points.isEmpty()) {
            return
        }
        val clientBatchId = "batch_${UUID.randomUUID().toString().replace("-", "")}"
        seenBatchIds.add(clientBatchId)
        strokes.add(stroke)
        socket?.commitBatch(clientBatchId, listOf(stroke))
    }

    private fun handle(event: PageEvent) {
        when (event) {
            is PageEvent.Welcome -> {
                sessionId = event.sessionId
                statusBanner = null
                // Catch up on persisted ink, then take the lease if it is free.
                socket?.subscribe(lastSeq)
                if (event.leaseHolder == null || event.leaseHolder == sessionId) {
                    socket?.acquireLease()
                } else {
                    canEdit = false
                    statusBanner = LEASE_BLOCKED
                }
            }
            is PageEvent.StrokeBatch -> {
                if (seenBatchIds.add(event.clientBatchId)) {
                    strokes.addAll(event.strokes)
                }
                if (event.seq > lastSeq) {
                    lastSeq = event.seq
                }
            }
            is PageEvent.Synced -> {
                if (event.lastSeq > lastSeq) {
                    lastSeq = event.lastSeq
                }
            }
            PageEvent.LeaseGranted -> {
                canEdit = true
                if (statusBanner == LEASE_BLOCKED) {
                    statusBanner = null
                }
            }
            is PageEvent.LeaseDenied -> {
                canEdit = false
                statusBanner = LEASE_BLOCKED
            }
            is PageEvent.LeaseChanged -> {
                val holder = event.holder
                when {
                    // Lease freed elsewhere — try to take it so this editor can ink.
                    holder == null -> socket?.acquireLease()
                    holder == sessionId -> {
                        canEdit = true
                        if (statusBanner == LEASE_BLOCKED) {
                            statusBanner = null
                        }
                    }
                    else -> {
                        canEdit = false
                        statusBanner = LEASE_BLOCKED
                    }
                }
            }
            is PageEvent.Failure -> statusBanner = event.message
        }
    }

    companion object {
        const val LEASE_BLOCKED = "Another device is editing this page"
        const val CONNECTING = "Connecting…"
        const val DISCONNECTED = "Realtime disconnected"
        const val NOT_SIGNED_IN = "Not signed in"
    }
}
