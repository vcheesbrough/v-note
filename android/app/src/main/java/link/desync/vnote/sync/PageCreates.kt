package link.desync.vnote.sync

import link.desync.vnote.api.ApiClient
import link.desync.vnote.api.AuthUnavailableException
import link.desync.vnote.api.HttpStatusException
import link.desync.vnote.api.SessionExpiredException
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
    // false when one must wait — offline, or sync paused for a sign-in.
    suspend fun push(onCreated: (PageSummary) -> Unit): Boolean =
        store.outbox.read
            .pendingCreates()
            .all { op -> pushOne(op, onCreated) }

    // One create. True when the next may go on: it landed, or was refused for
    // good. False when it must wait.
    private suspend fun pushOne(
        op: OutboxOp,
        onCreated: (PageSummary) -> Unit,
    ): Boolean {
        val result = apiClient.createPage(title = op.title.ifBlank { null }, paper = op.paper, id = op.pageId)
        val created = result.getOrNull() ?: return handleFailure(op.pageId, result.exceptionOrNull())
        if (created.id != op.pageId) {
            // A server from before #159 ignores the id and picks its own:
            // everything owed is keyed by ours, so nothing may follow it
            // there. Refused, and kept for the user to recover.
            AppLog.w(TAG, "page create answered with a different id; the server does not take device-made ids")
            store.outbox.reject(op.pageId, ID_MISMATCH, "The server does not accept pages made offline")
            status.refresh()
            return true
        }
        store.transaction {
            store.library.markCreated(created)
            store.outbox.acknowledge(op.opId)
        }
        AppLog.i(TAG, "page created on the server from this device's copy")
        status.reportReachable()
        status.markSynced()
        status.refresh()
        onCreated(created)
        return true
    }

    // True when the failure was definitive and the next create may go on.
    private fun handleFailure(
        pageId: String,
        failure: Throwable?,
    ): Boolean {
        when {
            failure is SessionExpiredException -> status.requireSignIn()
            failure is AuthUnavailableException -> status.reportFailure(apiClient.signInHost, ErrorClass.OFFLINE, null)
            failure is HttpStatusException && failure.code in DEFINITIVE -> {
                AppLog.w(TAG, "page create refused: HTTP ${failure.code}")
                val code = if (failure.code == HTTP_GONE) PAGE_DELETED else ErrorClass.http(failure.code)
                store.outbox.reject(pageId, code, failure.message ?: "The server refused this page")
                status.refresh()
                return true
            }
            failure is HttpStatusException ->
                status.reportFailure(hostOf(apiClient.baseUrl), ErrorClass.http(failure.code), failure.requestId)
            failure is java.io.IOException ->
                status.reportFailure(hostOf(apiClient.baseUrl), ErrorClass.OFFLINE, null)
            // "Not signed in": no token at all.
            else -> status.requireSignIn()
        }
        return false
    }

    private companion object {
        const val TAG = "VNoteSync"
        const val HTTP_GONE = 410
        const val PAGE_DELETED = "page_deleted"
        const val ID_MISMATCH = "id_mismatch"

        // Bad request, not this user's, not found, conflict, purged: no
        // replay of the same create can succeed.
        val DEFINITIVE = setOf(400, 403, 404, 409, HTTP_GONE)
    }
}
