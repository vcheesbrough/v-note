package link.desync.vnote.store

import android.content.ContentValues
import android.database.Cursor
import link.desync.vnote.ink.Paper
import link.desync.vnote.model.PageSummary
import link.desync.vnote.model.ThumbnailMetadata
import org.json.JSONObject

// The library as the device last saw it, so it opens offline (#159), plus
// the pages created here that the server has not acknowledged yet. The list
// itself is the library holder's; this mirrors it.
class LibraryCache internal constructor(
    private val store: LocalStore,
) {
    // Every page, most recently updated first.
    fun pages(): List<PageSummary> =
        store.db.rawQuery("SELECT $COLUMNS FROM $PAGES ORDER BY updated_at DESC, created_at DESC", null).use { cursor ->
            buildList { while (cursor.moveToNext()) add(cursor.toPage()) }
        }

    // Mirrors the library's list: rows not in [pages] go, the rest are
    // written. A page created here and not yet acknowledged is never dropped,
    // whatever the list says — it is not on the server to be listed yet.
    fun write(pages: List<PageSummary>) {
        store.transaction { db ->
            val keep = pages.map { it.id }.toSet()
            val existing =
                db.rawQuery("SELECT id FROM $PAGES WHERE local_only = 0", null).use { cursor ->
                    buildList { while (cursor.moveToNext()) add(cursor.getString(0)) }
                }
            existing.filterNot { it in keep }.forEach { db.delete(PAGES, "id = ?", arrayOf(it)) }
            pages.forEach { upsert(it) }
        }
    }

    // A page made on this device, offline or not; the create goes out through
    // the outbox ([SyncOutbox.enqueueCreate] in the same transaction).
    internal fun insertLocal(page: PageSummary) {
        store.db.insertOrThrow(PAGES, null, page.toValues().apply { put("local_only", 1) })
    }

    // The server acknowledged the create; its timestamps win.
    internal fun markCreated(page: PageSummary) {
        val values = page.toValues().apply { put("local_only", 0) }
        if (store.db.update(PAGES, values, "id = ?", arrayOf(page.id)) == 0) {
            store.db.insertOrThrow(PAGES, null, values)
        }
    }

    // A page made here whose create the server refused, once the user has
    // kept its ink as a new page or let it go.
    internal fun removeLocal(pageId: String) {
        store.db.delete(PAGES, "id = ? AND local_only = 1", arrayOf(pageId))
    }

    fun localOnlyIds(): Set<String> =
        store.db.rawQuery("SELECT id FROM $PAGES WHERE local_only = 1", null).use { cursor ->
            buildSet { while (cursor.moveToNext()) add(cursor.getString(0)) }
        }

    fun isLocalOnly(pageId: String): Boolean = pageId in localOnlyIds()

    // The pages whose ink has changed since it was last fetched in full.
    fun staleInk(): List<PageSummary> =
        store.db
            .rawQuery(
                "SELECT $COLUMNS FROM $PAGES WHERE local_only = 0 AND (ink_synced_at IS NULL OR ink_synced_at <> updated_at) " +
                    "ORDER BY updated_at DESC",
                null,
            ).use { cursor -> buildList { while (cursor.moveToNext()) add(cursor.toPage()) } }

    fun markInkSynced(
        pageId: String,
        updatedAt: String,
    ) {
        store.transaction { db ->
            db.update(PAGES, ContentValues().apply { put("ink_synced_at", updatedAt) }, "id = ?", arrayOf(pageId))
        }
    }

    // Keeps `local_only` and `ink_synced_at`: neither is the server's to say.
    // Update-then-insert because SQLite's UPSERT needs API 30.
    private fun upsert(page: PageSummary) {
        val values = page.toValues()
        if (store.db.update(PAGES, values, "id = ?", arrayOf(page.id)) == 0) {
            store.db.insertOrThrow(PAGES, null, values)
        }
    }

    private companion object {
        const val COLUMNS = "id, title, created_at, updated_at, paper, thumbnail"
    }
}

private fun PageSummary.toValues(): ContentValues =
    ContentValues().apply {
        put("id", id)
        put("title", title)
        put("created_at", createdAt)
        put("updated_at", updatedAt)
        put("paper", paper.wireValue)
        put("thumbnail", encodeThumbnail(thumbnail))
    }

private fun Cursor.toPage(): PageSummary =
    PageSummary(
        id = getString(0),
        title = getString(1),
        createdAt = getString(2),
        updatedAt = getString(3),
        paper = Paper.fromWire(getString(4)) ?: Paper.None,
        thumbnail = if (isNull(5)) ThumbnailMetadata.Empty else decodeThumbnail(getString(5)),
    )

internal fun encodeThumbnail(thumbnail: ThumbnailMetadata): String =
    when (thumbnail) {
        ThumbnailMetadata.Empty -> JSONObject().put("status", "empty")
        is ThumbnailMetadata.Generating -> JSONObject().put("status", "generating").put("source_seq", thumbnail.sourceSeq)
        is ThumbnailMetadata.Available ->
            JSONObject().put("status", "available").put("source_seq", thumbnail.sourceSeq).put("url", thumbnail.url)
        is ThumbnailMetadata.Failed -> JSONObject().put("status", "failed").put("source_seq", thumbnail.sourceSeq)
    }.toString()

private fun decodeThumbnail(json: String): ThumbnailMetadata =
    runCatching {
        link.desync.vnote.api.codec
            .decodeThumbnail(JSONObject(json))
    }.getOrDefault(ThumbnailMetadata.Empty)
