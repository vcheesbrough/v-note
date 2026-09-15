package link.desync.vnote.api

import link.desync.vnote.api.codec.encodeAcquireLease
import link.desync.vnote.api.codec.encodeCommitBatch
import link.desync.vnote.api.codec.encodeCommitTombstones
import link.desync.vnote.api.codec.encodeReleaseLease
import link.desync.vnote.api.codec.encodeRenewLease
import link.desync.vnote.api.codec.encodeSetPaper
import link.desync.vnote.api.codec.encodeSubscribe
import link.desync.vnote.ink.Paper
import link.desync.vnote.model.Stroke
import okhttp3.WebSocket

// Handle for sending client → server messages on an open page channel.
class PageSocket(
    private val webSocket: WebSocket,
) {
    fun subscribe(fromSeq: Long) {
        webSocket.send(encodeSubscribe(fromSeq))
    }

    fun acquireLease() {
        webSocket.send(encodeAcquireLease())
    }

    fun renewLease() {
        webSocket.send(encodeRenewLease())
    }

    fun releaseLease() {
        webSocket.send(encodeReleaseLease())
    }

    fun commitBatch(
        clientBatchId: String,
        strokes: List<Stroke>,
    ) {
        webSocket.send(encodeCommitBatch(clientBatchId, strokes))
    }

    fun setPaper(
        clientMutationId: String,
        paper: Paper,
    ) {
        webSocket.send(encodeSetPaper(clientMutationId, paper))
    }

    fun commitTombstones(
        clientMutationId: String,
        strokeIds: List<String>,
    ) {
        webSocket.send(encodeCommitTombstones(clientMutationId, strokeIds))
    }

    fun close() {
        webSocket.close(1000, "page closed")
    }
}
