package link.desync.vnote.sync

import link.desync.vnote.ink.Paper
import link.desync.vnote.model.PageSummary
import link.desync.vnote.store.Edit
import link.desync.vnote.store.LocalStore
import link.desync.vnote.store.OpKind
import java.time.Instant
import java.util.UUID

// What the user can do with edits owed to a page that was permanently deleted
// elsewhere (#651): keep them as a new page. The device may hold the only
// copy, and on the deleted page they can never land. Optional — left alone
// they stay owed and retried — and there is no way to let them go.
class PageRecovery internal constructor(
    private val store: LocalStore,
) {
    // A new page holding the owed strokes, under fresh ids so none of them
    // collides with what the server already holds. Created locally and
    // synced like any page made here. Returns it, so the caller can open it.
    fun keepAsNewPage(pageId: String): PageSummary {
        // Everything the page still owes.
        val owed =
            store.outbox.read
                .forPage(pageId)
                .filter { it.kind != OpKind.CreatePage }
        val erased = owed.filter { it.kind == OpKind.Erase }.flatMap { it.strokeIds }.toSet()
        val batches =
            owed
                .filter { it.kind == OpKind.CommitBatch }
                .map { op -> op.strokes.filterNot { it.id in erased } }
                .filter { it.isNotEmpty() }
        val paper = owed.lastOrNull { it.kind == OpKind.SetPaper }?.paper ?: store.ink.load(pageId).paper ?: Paper.None
        val now = Instant.now().toString()
        val page = PageSummary(id = "page_${newId()}", title = "", createdAt = now, updatedAt = now, paper = paper)
        store.transaction {
            store.outbox.createPage(page)
            batches.forEach { strokes ->
                store.outbox.enqueue(page.id, Edit.Batch("batch_${newId()}", strokes.map { it.copy(id = "stroke_${newId()}") }))
            }
            forget(pageId)
        }
        return page
    }

    // The deleted page's entries go, and a page made here goes with them: it
    // never reached the server, and its ink is now on the new page.
    private fun forget(pageId: String) {
        store.outbox.discardPage(pageId)
        store.library.removeLocal(pageId)
    }

    private fun newId(): String = UUID.randomUUID().toString().replace("-", "")
}
