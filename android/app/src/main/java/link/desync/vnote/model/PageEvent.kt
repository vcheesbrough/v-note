package link.desync.vnote.model

import link.desync.vnote.ink.Paper

// Server → client messages on the page channel.
sealed interface PageEvent {
    data class Welcome(
        val sessionId: String,
        val lastSeq: Long,
        val leaseHolder: String?,
        val paper: Paper = Paper.None,
    ) : PageEvent

    data class PaperChanged(
        val paper: Paper,
        val revision: Long,
    ) : PageEvent

    data class StrokeBatch(
        val seq: Long,
        val clientBatchId: String,
        val strokes: List<Stroke>,
    ) : PageEvent

    // The whole `subscribe` replay in one frame (#323): surviving batches with
    // delete-wins already applied, every tombstone batch, and the head `seq`
    // that used to arrive as a separate `synced`.
    data class PageReplay(
        val pageId: String,
        val lastSeq: Long,
        val batches: List<StrokeBatch>,
        val tombstones: List<TombstoneBatch>,
    ) : PageEvent

    data class Synced(
        val lastSeq: Long,
    ) : PageEvent

    data class TombstoneBatch(
        val revision: Long,
        val clientMutationId: String,
        val strokeIds: List<String>,
    ) : PageEvent

    data object LeaseGranted : PageEvent

    data class LeaseDenied(
        val holder: String,
    ) : PageEvent

    data class LeaseChanged(
        val holder: String?,
    ) : PageEvent

    data class Failure(
        val code: String,
        val message: String,
        val clientMutationId: String?,
    ) : PageEvent
}
