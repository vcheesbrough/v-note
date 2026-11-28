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
    // Sends every create still owed, oldest first. True once none is left;
    // false when any must be tried again. Every one is tried on every pass,
    // so a create that keeps failing never holds the others back.
    suspend fun push(onCreated: (PageSummary) -> Unit): Boolean {
        var allLanded = true
        for (op in store.outbox.read.pendingCreates()) {
            if (!pushOne(op, onCreated)) allLanded = false
        }
        return allLanded
    }

    // One create. True when it landed. Any failure leaves it owed (#651).
    private suspend fun pushOne(
        op: OutboxOp,
        onCreated: (PageSummary) -> Unit,
    ): Boolean {
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
            return false
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
        return true
    }

    private fun failed(
        pageId: String,
        failure: Throwable?,
    ): Boolean {
        AppLog.w(TAG, "page create failed; it stays owed: ${failure?.javaClass?.simpleName}")
        status.reportFailure(failure, hostOf(apiClient.baseUrl), apiClient.signInHost)
        if (failure is HttpStatusException && ErrorClass.isGone(ErrorClass.http(failure.code))) status.reportGone(pageId)
        return false
    }

    private companion object {
        const val TAG = "VNoteSync"
    }
}
