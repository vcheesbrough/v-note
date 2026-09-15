package link.desync.vnote.api.codec

import link.desync.vnote.ink.Paper
import link.desync.vnote.model.LibraryEvent
import link.desync.vnote.model.MeProfile
import link.desync.vnote.model.PageSummary
import link.desync.vnote.model.ThumbnailMetadata
import org.json.JSONObject

// JSON codecs for the REST resources and the library channel. `internal` rather
// than private so the JVM unit tests can hold every one against
// `contracts/fixtures/` (see ContractFixtureTest).

internal fun decodeMe(json: JSONObject): MeProfile =
    MeProfile(
        sub = json.getString("sub"),
        email = json.optString("email").ifBlank { null },
    )

internal fun decodePage(json: JSONObject): PageSummary =
    PageSummary(
        id = json.getString("id"),
        title = json.getString("title"),
        createdAt = json.getString("created_at"),
        updatedAt = json.getString("updated_at"),
        thumbnail = json.optJSONObject("thumbnail")?.let(::decodeThumbnail) ?: ThumbnailMetadata.Empty,
        // Absent on pre-v5 payloads, which render as blank pages.
        paper = Paper.fromWire(json.optString("paper").ifBlank { null }) ?: Paper.None,
    )

internal fun decodePageList(json: JSONObject): List<PageSummary> {
    val pages = json.getJSONArray("pages")
    return buildList {
        for (index in 0 until pages.length()) {
            add(decodePage(pages.getJSONObject(index)))
        }
    }
}

internal fun decodeThumbnail(json: JSONObject): ThumbnailMetadata =
    when (json.getString("status")) {
        "empty" -> ThumbnailMetadata.Empty
        "generating" -> ThumbnailMetadata.Generating(json.getLong("source_seq"))
        "available" -> ThumbnailMetadata.Available(json.getLong("source_seq"), json.getString("url"))
        "failed" -> ThumbnailMetadata.Failed(json.getLong("source_seq"))
        else -> error("Unknown thumbnail status")
    }

internal fun decodeLibraryEvent(json: JSONObject): LibraryEvent =
    when (val type = json.getString("type")) {
        "page-created" -> LibraryEvent.PageCreated(decodePage(json.getJSONObject("page")))
        "page-deleted" -> LibraryEvent.PageDeleted(json.getString("page_id"))
        "page-thumbnail-updated" ->
            LibraryEvent.PageThumbnailUpdated(
                json.getString("page_id"),
                decodeThumbnail(json.getJSONObject("thumbnail")),
            )
        "page-updated" ->
            LibraryEvent.PageUpdated(
                json.getString("page_id"),
                json.getString("updated_at"),
            )
        else -> error("Unknown realtime event type: $type")
    }

// The paper is sent on create rather than as a post-create set-paper: one round
// trip, no revision bump. A null title is omitted, not sent as `null`.
internal fun encodeCreatePage(
    title: String?,
    paper: Paper,
): String =
    JSONObject()
        .apply {
            title?.let { put("title", it) }
            put("paper", paper.wireValue)
        }.toString()
