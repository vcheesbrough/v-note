package link.desync.vnote.sync

import link.desync.vnote.api.ApiClient
import link.desync.vnote.api.HttpStatusException
import link.desync.vnote.api.hostOf
import link.desync.vnote.model.PageSummary
import link.desync.vnote.store.LocalStore
import link.desync.vnote.store.OutboxOp
import link.desync.vnote.telemetry.AppLog

// Replays the pages created on this device (#159): each create carries the
// page id minted here, so the server's create is idempotent on it and a
// replay after a lost answer returns the page it already made.
internal class PageCreates(
    private val apiClient: ApiClient,
    private val store: LocalStore,
    private val status: SyncStatus,
) {
    // Sends the creates still owed, oldest first. True once none is left;
    // false when any must be tried again. A create the server answered for
    // with an error does not hold the ones behind it back; a failure that
    // says nothing about the create itself — the server or the network is
    // away — ends the pass, since the rest would only fail the same way,
    // one timeout after another, ahead of the replays waiting behind them.
    suspend fun push(onCreated: (PageSummary) -> Unit): Boolean {
        var allLanded = true
        for (op in store.outbox.read.pendingCreates()) {
            when (pushOne(op, onCreated)) {
                Outcome.Landed -> Unit
                Outcome.Answered -> allLanded = false
                Outcome.Unreachable -> return false
            }
        }
        return allLanded
    }

    // One create. Any failure leaves it owed, to be retried (#651).
    private suspend fun pushOne(
        op: OutboxOp,
        onCreated: (PageSummary) -> Unit,
    ): Outcome {
        val result = apiClient.createPage(title = op.title.ifBlank { null }, paper = op.paper, id = op.pageId)
        val created = result.getOrNull() ?: return failed(op.pageId, result.exceptionOrNull())
        if (created.id != op.pageId) {
            // Not an answer v-note gives: its create is idempotent on the id
            // sent. Everything owed is keyed by ours, so nothing may follow
            // the page there. A failure like any other: owed, and retried.
            // (A server from before 0.85 ignored the id and would make a
            // page per retry; no environment runs one.)
            AppLog.w(TAG, "page create answered with a different id")
            status.reportFailure(hostOf(apiClient.baseUrl), ErrorClass.ID_MISMATCH, null)
            return Outcome.Answered
        }
        store.transaction {
            store.library.markCreated(created)
            store.outbox.acknowledge(op.opId)
        }
        AppLog.i(TAG, "page created on the server from this device's copy")
        status.reportReachable()
        status.reportPresent(op.pageId)
        status.markSynced()
        status.refresh()
        onCreated(created)
        return Outcome.Landed
    }

    private fun failed(
        pageId: String,
        failure: Throwable?,
    ): Outcome {
        AppLog.w(TAG, "page create failed; it stays owed: ${failure?.javaClass?.simpleName}")
        status.reportFailure(failure, hostOf(apiClient.baseUrl), apiClient.signInHost)
        if (failure !is HttpStatusException) return Outcome.Unreachable
        if (ErrorClass.isGone(ErrorClass.http(failure.code))) status.reportGone(pageId)
        return Outcome.Answered
    }

    // How one create went. Every outcome but [Landed] leaves it owed; they
    // differ only in whether the pass goes on to the next create.
    private enum class Outcome {
        Landed,

        // An HTTP status, or a page under another id: an answer for this
        // create, which says nothing about the next one.
        Answered,

        // No answer for this create at all: no network, no server, no
        // token, or a body that is not v-note's.
        Unreachable,
    }

    private companion object {
        const val TAG = "VNoteSync"
    }
}
