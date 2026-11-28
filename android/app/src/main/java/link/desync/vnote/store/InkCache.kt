package link.desync.vnote.store

import android.content.ContentValues
import android.database.sqlite.SQLiteDatabase
import link.desync.vnote.ink.Paper
import link.desync.vnote.model.Stroke
import org.json.JSONArray

// One page's confirmed ink as the device holds it: the server's batches in
// server order, the stroke ids it erased, and its replay cursor.
data class CachedInk(
    val batches: List<ConfirmedBatch>,
    val tombstones: Set<String>,
    val lastSeq: Long,
    // The paper the server last confirmed, or null before the first connect.
    val paper: Paper?,
)

data class ConfirmedBatch(
    val seq: Long,
    val clientBatchId: String,
    val strokes: List<Stroke>,
)

// Server-confirmed ink per page (#159), so every page opens offline. Each
// confirmation also acknowledges the outbox entry it answers, in the same
// transaction: an acknowledged edit is always confirmed ink, never neither.
class InkCache internal constructor(
    private val store: LocalStore,
) {
    fun load(pageId: String): CachedInk {
        val db = store.db
        val batches =
            db.rawQuery("SELECT seq, client_batch_id, strokes FROM $BATCHES WHERE page_id = ? ORDER BY seq", arrayOf(pageId)).use {
                buildList {
                    while (it.moveToNext()) add(ConfirmedBatch(it.getLong(0), it.getString(1), JSONArray(it.getString(2)).toStrokes()))
                }
            }
        val tombstones =
            db.rawQuery("SELECT stroke_id FROM $TOMBSTONES WHERE page_id = ?", arrayOf(pageId)).use {
                buildSet { while (it.moveToNext()) add(it.getString(0)) }
            }
        return db.rawQuery("SELECT last_seq, paper FROM $PAGE_INK WHERE page_id = ?", arrayOf(pageId)).use { cursor ->
            val found = cursor.moveToFirst()
            CachedInk(
                batches = batches,
                tombstones = tombstones,
                lastSeq = if (found) cursor.getLong(0) else 0,
                paper = if (found && !cursor.isNull(1)) Paper.fromWire(cursor.getString(1)) else null,
            )
        }
    }

    // A batch the server holds — this device's echo, a replayed one, or
    // another device's — and the outbox entry it acknowledges, if ours.
    fun confirmBatch(
        pageId: String,
        batch: ConfirmedBatch,
    ) {
        store.transaction { db ->
            val values =
                ContentValues().apply {
                    put("page_id", pageId)
                    put("client_batch_id", batch.clientBatchId)
                    put("seq", batch.seq)
                    put("strokes", batch.strokes.toJson().toString())
                }
            db.insertWithOnConflict(BATCHES, null, values, SQLiteDatabase.CONFLICT_IGNORE)
            advance(db, pageId, batch.seq)
            store.outbox.acknowledge(batch.clientBatchId)
        }
    }

    // Stroke ids the server erased, and the erase entry it acknowledges.
    fun confirmTombstones(
        pageId: String,
        clientMutationId: String,
        strokeIds: Collection<String>,
    ) {
        store.transaction { db ->
            strokeIds.forEach { strokeId ->
                val values =
                    ContentValues().apply {
                        put("page_id", pageId)
                        put("stroke_id", strokeId)
                    }
                db.insertWithOnConflict(TOMBSTONES, null, values, SQLiteDatabase.CONFLICT_IGNORE)
            }
            store.outbox.acknowledge(clientMutationId)
        }
    }

    // The paper in force on the server. [acknowledged] are the paper entries
    // it answers: last arrival wins, so the change now in force settles every
    // earlier one this device made too.
    fun confirmPaper(
        pageId: String,
        paper: Paper,
        acknowledged: Collection<String> = emptyList(),
    ) {
        store.transaction { db ->
            ensureRow(db, pageId)
            db.update(PAGE_INK, ContentValues().apply { put("paper", paper.wireValue) }, "page_id = ?", arrayOf(pageId))
            acknowledged.forEach(store.outbox::acknowledge)
        }
    }

    fun advance(
        pageId: String,
        seq: Long,
    ) {
        store.transaction { db -> advance(db, pageId, seq) }
    }

    // The page was permanently deleted: its confirmed ink goes with it. Any
    // outbox entries stay, to be rejected and offered as a new page.
    fun forget(pageId: String) {
        store.transaction { db ->
            listOf(BATCHES, TOMBSTONES, PAGE_INK).forEach { table -> db.delete(table, "page_id = ?", arrayOf(pageId)) }
        }
    }

    private fun advance(
        db: SQLiteDatabase,
        pageId: String,
        seq: Long,
    ) {
        ensureRow(db, pageId)
        db.execSQL("UPDATE $PAGE_INK SET last_seq = MAX(last_seq, ?) WHERE page_id = ?", arrayOf<Any>(seq, pageId))
    }

    private fun ensureRow(
        db: SQLiteDatabase,
        pageId: String,
    ) {
        db.insertWithOnConflict(PAGE_INK, null, ContentValues().apply { put("page_id", pageId) }, SQLiteDatabase.CONFLICT_IGNORE)
    }
}
