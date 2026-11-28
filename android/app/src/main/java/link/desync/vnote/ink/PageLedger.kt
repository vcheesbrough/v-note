package link.desync.vnote.ink

import link.desync.vnote.model.PageEvent
import link.desync.vnote.model.Stroke
import link.desync.vnote.store.ConfirmedBatch
import link.desync.vnote.store.Edit
import link.desync.vnote.store.LocalStore
import link.desync.vnote.store.OpKind
import link.desync.vnote.store.OpState
import link.desync.vnote.store.OutboxOp

// One page's ink as the device holds it (#159): what the server confirmed,
// and what this device drew, erased or chose since that the server has not
// acknowledged. Every edit reaches the durable outbox before it reaches
// memory, and every confirmation lands in the store before it changes what
// is drawn, so a process killed at any point restarts into the same page.
// [PageInkSession] drives the socket; this keeps the books.
internal class PageLedger(
    private val store: LocalStore,
    private val pageId: String,
) {
    // Confirmed strokes in server arrival order, erased ones already removed.
    private val confirmed = mutableListOf<Stroke>()
    private val seenBatchIds = hashSetOf<String>()
    private val tombstones = hashSetOf<String>()

    // Unacknowledged edits by their immutable id, in the order they were made.
    val pendingBatches = linkedMapOf<String, List<Stroke>>()
    val pendingErasures = linkedMapOf<String, Set<String>>()
    private val pendingPaper = linkedMapOf<String, Paper>()

    // Edits the server definitively refused: kept and drawn until the user
    // recovers or discards them, never counted as synced.
    private val rejectedBatches = linkedMapOf<String, List<Stroke>>()

    var lastSeq = 0L
        private set

    var confirmedPaper: Paper? = null
        private set

    // Why the server refused this page's edits, or null.
    var rejection: Pair<String, String>? = null
        private set

    // The newest paper this device asked for and the server has not confirmed.
    val requestedPaper: Paper? get() = pendingPaper.values.lastOrNull()

    val hasWork: Boolean
        get() = pendingBatches.isNotEmpty() || pendingErasures.isNotEmpty() || pendingPaper.isNotEmpty()

    val unsentStrokeCount: Int get() = pendingBatches.values.sumOf { it.size }

    // Restores the page from the store: its confirmed ink and every edit still
    // on its way, so a page opened offline — or after a crash — is the page
    // the user left.
    fun load() {
        val cached = store.ink.load(pageId)
        tombstones += cached.tombstones
        cached.batches.forEach { batch ->
            seenBatchIds += batch.clientBatchId
            confirmed += batch.strokes.filterNot { it.id in tombstones }
        }
        lastSeq = cached.lastSeq
        confirmedPaper = cached.paper
        for (op in store.outbox.read.forPage(pageId)) {
            if (op.state == OpState.Rejected) {
                rejection = op.detailCode.orEmpty() to op.detailMessage.orEmpty()
                if (op.kind == OpKind.CommitBatch) rejectedBatches[op.opId] = op.strokes
                continue
            }
            when (op.kind) {
                OpKind.CommitBatch -> pendingBatches[op.opId] = op.strokes
                OpKind.Erase -> pendingErasures[op.opId] = op.strokeIds.toSet()
                OpKind.SetPaper -> pendingPaper[op.opId] = op.paper
                OpKind.CreatePage -> Unit
            }
        }
    }

    fun record(edit: Edit) {
        store.outbox.enqueue(pageId, edit)
        rejection?.let { (code, message) ->
            // The page is refused: a new edit cannot land either, so it joins
            // the refusal and goes wherever the user sends the rest.
            store.outbox.reject(pageId, code, message)
            if (edit is Edit.Batch) rejectedBatches[edit.clientBatchId] = edit.strokes
            return
        }
        when (edit) {
            is Edit.Batch -> pendingBatches[edit.clientBatchId] = edit.strokes
            is Edit.Erase -> pendingErasures[edit.clientMutationId] = edit.strokeIds.toSet()
            is Edit.SetPaper -> pendingPaper[edit.clientMutationId] = edit.paper
        }
    }

    // Everything still to send, in the order it was made — the per-page
    // replay order. Read from the store, which is the record of what is owed.
    fun replayable(): List<OutboxOp> =
        store.outbox.read.forPage(pageId).filter {
            it.state != OpState.Rejected &&
                it.kind != OpKind.CreatePage
        }

    // A batch the server holds. True when it is new to this page. Ours or not,
    // it is stored first, acknowledging our entry in the same transaction.
    fun confirmBatch(event: PageEvent.StrokeBatch): Boolean {
        store.ink.confirmBatch(pageId, ConfirmedBatch(event.seq, event.clientBatchId, event.strokes))
        if (event.seq > lastSeq) lastSeq = event.seq
        pendingBatches.remove(event.clientBatchId)
        if (!seenBatchIds.add(event.clientBatchId)) return false
        confirmed += event.strokes.filterNot { it.id in tombstones }
        return true
    }

    // Strokes the server erased, and the batches that settles: a pending batch
    // whose every stroke is now erased can never be echoed — the server
    // suppresses an add of an erased stroke (delete wins) — so it is settled
    // here rather than replayed forever. Returns those batch ids.
    fun confirmTombstones(event: PageEvent.TombstoneBatch): Set<String> {
        val erased = pendingErasures.remove(event.clientMutationId).orEmpty() + event.strokeIds
        store.ink.confirmTombstones(pageId, event.clientMutationId, erased)
        tombstones += erased
        confirmed.removeAll { it.id in erased }
        val settled = pendingBatches.filterValues { strokes -> strokes.all { it.id in tombstones } }.keys.toSet()
        settled.forEach { batchId ->
            pendingBatches.remove(batchId)
            store.outbox.acknowledge(batchId)
        }
        return settled
    }

    // The paper in force on the server. True when it is the one this device
    // last asked for: last arrival wins, so that settles every earlier ask too.
    fun confirmPaper(paper: Paper): Boolean {
        confirmedPaper = paper
        val answered = requestedPaper == paper
        val acknowledged = if (answered) pendingPaper.keys.toList() else emptyList()
        if (answered) pendingPaper.clear()
        store.ink.confirmPaper(pageId, paper, acknowledged)
        return answered
    }

    fun advance(seq: Long) {
        if (seq <= lastSeq) return
        lastSeq = seq
        store.ink.advance(pageId, seq)
    }

    // The server refused this page for good: everything still owed is
    // rejected with it, kept, and drawn until the user decides.
    fun reject(
        code: String,
        message: String,
    ) {
        store.outbox.reject(pageId, code, message)
        rejectedBatches.putAll(pendingBatches)
        pendingBatches.clear()
        pendingErasures.clear()
        pendingPaper.clear()
        rejection = code to message
    }

    // What to draw: confirmed ink, then this device's unconfirmed and refused
    // ink on top, less anything erased here and not yet confirmed.
    fun renderable(): List<Stroke> {
        val erasing = pendingErasures.values.flatten().toSet()
        val local = (pendingBatches.values + rejectedBatches.values).flatten().filterNot { it.id in tombstones }
        val all = confirmed + local
        return if (erasing.isEmpty()) all else all.filterNot { it.id in erasing }
    }
}
