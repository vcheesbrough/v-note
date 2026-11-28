package link.desync.vnote.store

import android.content.ContentValues
import android.database.Cursor
import link.desync.vnote.api.codec.decodeStroke
import link.desync.vnote.api.codec.encodeStroke
import link.desync.vnote.ink.Paper
import link.desync.vnote.model.PageSummary
import link.desync.vnote.model.Stroke
import org.json.JSONArray
import org.json.JSONObject

// What an outbox entry asks the server to do.
enum class OpKind(
    internal val wire: String,
) {
    CreatePage("create_page"),
    CommitBatch("commit_batch"),
    Erase("erase"),
    SetPaper("set_paper"),
    ;

    internal companion object {
        fun of(wire: String): OpKind = entries.first { it.wire == wire }
    }
}

// Where an entry is on its way to the server (#159). `Pending` was never
// sent; `Sent` went out on a socket that may have dropped before the answer.
// Both are replayed until `Acknowledged`, which the server's own echo sets.
// There is no other way out (#651): no failure, whatever it is, ends an
// entry — it stays owed and is retried until the server takes it.
enum class OpState(
    internal val wire: String,
) {
    Pending("pending"),
    Sent("sent"),
    Acknowledged("acknowledged"),
    ;

    internal companion object {
        fun of(wire: String): OpState = entries.first { it.wire == wire }
    }
}

data class OutboxOp(
    val position: Long,
    val opId: String,
    val pageId: String,
    val kind: OpKind,
    val state: OpState,
    internal val payload: JSONObject,
) {
    val strokes: List<Stroke>
        get() = payload.optJSONArray("strokes").toStrokes()

    val strokeIds: List<String>
        get() = payload.optJSONArray("stroke_ids").toStrings()

    val paper: Paper
        get() = Paper.fromWire(payload.optString("paper").ifBlank { null }) ?: Paper.None

    val title: String
        get() = payload.optString("title")
}

// The durable outbox (#159): every edit is written here, in one transaction,
// before the canvas reports it accepted, and stays until the server
// acknowledges it. A process kill, a reboot or a week offline loses nothing.
class SyncOutbox internal constructor(
    private val store: LocalStore,
) {
    // One edit, made on [pageId] under its immutable id.
    fun enqueue(
        pageId: String,
        edit: Edit,
    ): OutboxOp =
        when (edit) {
            is Edit.Batch -> insert(pageId, OpKind.CommitBatch, edit.clientBatchId, JSONObject().put("strokes", edit.strokes.toJson()))
            is Edit.Erase ->
                insert(pageId, OpKind.Erase, edit.clientMutationId, JSONObject().put("stroke_ids", JSONArray(edit.strokeIds.toList())))
            is Edit.SetPaper -> insert(pageId, OpKind.SetPaper, edit.clientMutationId, JSONObject().put("paper", edit.paper.wireValue))
        }

    // A page made on this device: the page row and its create, together. The
    // page's own id is the create's idempotency key on the server.
    fun createPage(page: PageSummary) {
        store.transaction {
            store.library.insertLocal(page)
            insert(page.id, OpKind.CreatePage, page.id, JSONObject().put("title", page.title).put("paper", page.paper.wireValue))
        }
    }

    fun markSent(opId: String) = setState(opId, OpState.Sent)

    // The server's answer for this entry arrived. [InkCache] calls this inside
    // the transaction that stores the confirmed ink, so the two never part.
    fun acknowledge(opId: String) = setState(opId, OpState.Acknowledged)

    // The user kept a permanently deleted page's edits as a new page: the
    // entries the deleted page still owed, which can never land, go.
    fun discardPage(pageId: String) {
        store.transaction { db -> db.delete(OUTBOX, "page_id = ? AND state <> ?", arrayOf(pageId, OpState.Acknowledged.wire)) }
    }

    // Acknowledged rows have done their job; kept only until the next start so
    // their final state is inspectable meanwhile.
    fun pruneAcknowledged() {
        store.transaction { db -> db.delete(OUTBOX, "state = ?", arrayOf(OpState.Acknowledged.wire)) }
    }

    private fun insert(
        pageId: String,
        kind: OpKind,
        opId: String,
        payload: JSONObject,
    ): OutboxOp =
        store.transaction { db ->
            val values =
                ContentValues().apply {
                    put("op_id", opId)
                    put("page_id", pageId)
                    put("kind", kind.wire)
                    put("payload", payload.toString())
                    put("state", OpState.Pending.wire)
                }
            val position = db.insertOrThrow(OUTBOX, null, values)
            OutboxOp(position, opId, pageId, kind, OpState.Pending, payload)
        }

    private fun setState(
        opId: String,
        state: OpState,
    ) {
        store.transaction { db ->
            // Never moves an entry out of its final state: a resend of an
            // acknowledged one is ignored.
            db.update(
                OUTBOX,
                ContentValues().apply { put("state", state.wire) },
                "op_id = ? AND state IN (?, ?)",
                arrayOf(opId, OpState.Pending.wire, OpState.Sent.wire),
            )
        }
    }

    // The read side, split from the writes so each stays one concern.
    val read = OutboxQueries(store)
}

// The edits a page can make, each under the id the server is idempotent on.
sealed interface Edit {
    data class Batch(
        val clientBatchId: String,
        val strokes: List<Stroke>,
    ) : Edit

    data class Erase(
        val clientMutationId: String,
        val strokeIds: Collection<String>,
    ) : Edit

    data class SetPaper(
        val clientMutationId: String,
        val paper: Paper,
    ) : Edit
}

// What the outbox holds: per page, and overall.
class OutboxQueries internal constructor(
    private val store: LocalStore,
) {
    // Unacknowledged entries for one page, in the order they were made.
    fun forPage(pageId: String): List<OutboxOp> = query("page_id = ? AND state <> ?", arrayOf(pageId, OpState.Acknowledged.wire))

    fun pendingCreates(): List<OutboxOp> =
        query("kind = ? AND state IN (?, ?)", arrayOf(OpKind.CreatePage.wire, OpState.Pending.wire, OpState.Sent.wire))

    // The pages with edits still to replay, oldest first.
    fun pagesWithWork(): List<String> =
        store.db
            .rawQuery(
                "SELECT page_id FROM $OUTBOX WHERE state IN (?, ?) AND kind <> ? GROUP BY page_id ORDER BY MIN(position)",
                arrayOf(OpState.Pending.wire, OpState.Sent.wire, OpKind.CreatePage.wire),
            ).use { cursor -> buildList { while (cursor.moveToNext()) add(cursor.getString(0)) } }

    // Edits made here the server has not yet acknowledged — what the status
    // icon counts, and what a different identity signing in would delete.
    fun unsyncedCount(): Int =
        store.db
            .rawQuery(
                "SELECT COUNT(*) FROM $OUTBOX WHERE state <> ?",
                arrayOf(OpState.Acknowledged.wire),
            ).use { cursor -> if (cursor.moveToFirst()) cursor.getInt(0) else 0 }

    private fun query(
        where: String,
        args: Array<String>,
    ): List<OutboxOp> =
        store.db
            .rawQuery(
                "SELECT position, op_id, page_id, kind, state, payload FROM $OUTBOX " +
                    "WHERE $where ORDER BY position",
                args,
            ).use { cursor -> buildList { while (cursor.moveToNext()) add(cursor.toOp()) } }

    private fun Cursor.toOp(): OutboxOp =
        OutboxOp(
            position = getLong(0),
            opId = getString(1),
            pageId = getString(2),
            kind = OpKind.of(getString(3)),
            state = OpState.of(getString(4)),
            payload = JSONObject(getString(5)),
        )
}

internal fun List<Stroke>.toJson(): JSONArray = JSONArray().also { array -> forEach { array.put(encodeStroke(it)) } }

internal fun JSONArray?.toStrokes(): List<Stroke> =
    if (this == null) emptyList() else List(length()) { index -> decodeStroke(getJSONObject(index)) }

private fun JSONArray?.toStrings(): List<String> = if (this == null) emptyList() else List(length()) { index -> getString(index) }
