package link.desync.vnote.api.codec

import link.desync.vnote.ink.Paper
import link.desync.vnote.model.LibraryEvent
import link.desync.vnote.model.MeProfile
import link.desync.vnote.model.PageEvent
import link.desync.vnote.model.PageSummary
import link.desync.vnote.model.SOLID_ROUND_PRESSURE_STYLE_VERSION
import link.desync.vnote.model.SolidRoundParameters
import link.desync.vnote.model.Stroke
import link.desync.vnote.model.StrokePoint
import link.desync.vnote.model.StrokeStyle
import link.desync.vnote.model.ThumbnailMetadata
import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.File

/**
 * Every contract fixture, held against the Android codecs on the JVM (#337).
 *
 * Server payloads are decoded and compared with the model the app builds from
 * them; client payloads are encoded by the app and compared structurally with
 * the fixture. [everyContractFixtureIsAccountedFor] fails when a fixture is
 * added without an Android assertion, the way `crates/protocol/tests/schemas.rs`
 * fails when one is added without a schema.
 */
class ContractFixtureTest {
    @Test
    fun everyContractFixtureIsAccountedFor() {
        val onDisk =
            fixturesDir()
                .listFiles { file -> file.extension == "json" }
                .orEmpty()
                .map { it.name }
                .toSortedSet()
        assertEquals(ASSERTED_HERE + ASSERTED_ELSEWHERE, onDisk)
    }

    // ---- REST ----------------------------------------------------------------

    @Test
    fun meDecodes() {
        assertEquals(
            MeProfile(sub = "v-note-test-service-account", email = "test@example.com"),
            decodeMe(fixture("me.json")),
        )
    }

    @Test
    fun pageDecodes() {
        assertEquals(page(Paper.RuledMarginNarrow), decodePage(fixture("page.json")))
    }

    @Test
    fun pageListDecodes() {
        assertEquals(listOf(page(Paper.SquaredLarge)), decodePageList(fixture("pages.json")))
    }

    @Test
    fun createPageEncodesToTheFixture() {
        assertJsonEquals(fixture("create-page.json"), encodeCreatePage("Meeting notes", Paper.RuledWide))
    }

    @Test
    fun createPageWithoutATitleOmitsTheKey() {
        assertJsonEquals(JSONObject("""{"paper":"none"}"""), encodeCreatePage(null, Paper.None))
    }

    /** The app authenticates its sockets with a bearer token and never asks for a ticket; the shape is still pinned. */
    @Test
    fun realtimeTicketKeepsItsShape() {
        val json = fixture("realtime-ticket.json")
        assertTrue(json.getString("ticket").startsWith("ticket_"))
        assertTrue(json.getString("expires_at").isNotEmpty())
    }

    // ---- Library channel -----------------------------------------------------

    @Test
    fun libraryEventsDecode() {
        assertEquals(
            LibraryEvent.PageCreated(page(Paper.None)),
            decodeLibraryEvent(fixture("library-event-page-created.json")),
        )
        assertEquals(
            LibraryEvent.PageDeleted(PAGE_ID),
            decodeLibraryEvent(fixture("library-event-page-deleted.json")),
        )
        assertEquals(
            LibraryEvent.PageThumbnailUpdated(
                PAGE_ID,
                ThumbnailMetadata.Available(4, "/api/pages/$PAGE_ID/thumbnails/4"),
            ),
            decodeLibraryEvent(fixture("library-event-page-thumbnail-updated.json")),
        )
        assertEquals(
            LibraryEvent.PageUpdated(PAGE_ID, "2026-07-22T23:59:00Z"),
            decodeLibraryEvent(fixture("library-event-page-updated.json")),
        )
    }

    @Test
    fun everyThumbnailStatusDecodes() {
        assertEquals(ThumbnailMetadata.Empty, decodeThumbnail(JSONObject("""{"status":"empty"}""")))
        assertEquals(
            ThumbnailMetadata.Generating(2),
            decodeThumbnail(JSONObject("""{"status":"generating","source_seq":2}""")),
        )
        assertEquals(
            ThumbnailMetadata.Failed(2),
            decodeThumbnail(JSONObject("""{"status":"failed","source_seq":2}""")),
        )
    }

    // ---- Page channel: server → client ----------------------------------------

    @Test
    fun welcomeDecodes() {
        assertEquals(
            PageEvent.Welcome(
                sessionId = "session_abcdef0123456789",
                lastSeq = 2,
                leaseHolder = HOLDER,
                paper = Paper.SquaredSmall,
            ),
            decodePageEvent(fixture("page-server-welcome.json")),
        )
    }

    @Test
    fun leaseDeniedDecodes() {
        assertEquals(PageEvent.LeaseDenied(HOLDER), decodePageEvent(fixture("page-server-lease-denied.json")))
    }

    @Test
    fun paperChangedDecodes() {
        assertEquals(
            PageEvent.PaperChanged(Paper.RuledMarginNarrow, 3),
            decodePageEvent(fixture("page-server-paper-changed.json")),
        )
    }

    @Test
    fun strokeBatchMessageDecodes() {
        assertEquals(
            PageEvent.StrokeBatch(
                seq = 1,
                clientBatchId = BATCH_A,
                strokes = listOf(v1Stroke("stroke_server_1", point(10, 20, 0), point(30, 25, 16))),
            ),
            decodePageEvent(fixture("page-server-stroke-batch.json")),
        )
    }

    // The coalesced replay frame (#323): one message carrying every surviving
    // batch, every tombstone batch, and the head `seq`.
    @Test
    fun pageReplayMessageDecodes() {
        assertEquals(
            PageEvent.PageReplay(
                pageId = "page_01j00000000000000000000000",
                lastSeq = 2,
                batches =
                    listOf(
                        PageEvent.StrokeBatch(
                            seq = 1,
                            clientBatchId = BATCH_A,
                            strokes = listOf(v1Stroke("stroke_server_1", point(10, 20, 0), point(30, 25, 16))),
                        ),
                        PageEvent.StrokeBatch(
                            seq = 2,
                            clientBatchId = BATCH_B,
                            strokes = listOf(v1Stroke("stroke_server_2", point(5, 60, 0), point(80, 90, 16))),
                        ),
                    ),
                tombstones =
                    listOf(
                        PageEvent.TombstoneBatch(
                            revision = 1,
                            clientMutationId = "erase_cccccccccccccccccccccccc",
                            strokeIds = listOf("stroke_server_erased"),
                        ),
                    ),
            ),
            decodePageEvent(fixture("page-server-page-replay.json")),
        )
    }

    // A replay written before tombstones joined the snapshot still decodes,
    // with no tombstones rather than a parse failure.
    @Test
    fun pageReplayWithoutTombstonesDecodesToNone() {
        val decoded =
            decodePageEvent(
                JSONObject("""{"type":"page-replay","page_id":"page_1","last_seq":0,"batches":[]}"""),
            )
        assertEquals(PageEvent.PageReplay("page_1", 0, emptyList(), emptyList()), decoded)
    }

    @Test
    fun unknownServerMessagesAreIgnored() {
        assertNull(decodePageEvent(JSONObject("""{"type":"from-the-future"}""")))
    }

    // ---- Ink -------------------------------------------------------------------

    @Test
    fun strokeBatchDecodes() {
        assertEquals(
            PageEvent.StrokeBatch(
                seq = 1,
                clientBatchId = BATCH_A,
                strokes = listOf(v1Stroke("stroke_batch_1", point(10, 20, 0), point(30, 25, 16))),
            ),
            decodeStrokeBatch(fixture("stroke-batch.json")),
        )
    }

    @Test
    fun pageReplayBatchesDecode() {
        val replay = fixture("page-replay.json")
        val batchesJson = replay.getJSONArray("batches")
        val batches = (0 until batchesJson.length()).map { decodeStrokeBatch(batchesJson.getJSONObject(it)) }
        assertEquals(listOf(1L, 2L), batches.map { it.seq })
        assertEquals(
            listOf(listOf("stroke_replay_1"), listOf("stroke_replay_2", "stroke_replay_3")),
            batches.map { batch -> batch.strokes.map { it.id } },
        )
        assertEquals(replay.getLong("last_seq"), batches.last().seq)

        // Delete-wins is applied server-side, so nothing the tombstones name
        // may appear in the batches the client is handed.
        val tombstonesJson = replay.getJSONArray("tombstones")
        val erased =
            (0 until tombstonesJson.length())
                .flatMap { index ->
                    val ids = tombstonesJson.getJSONObject(index).getJSONArray("stroke_ids")
                    (0 until ids.length()).map { ids.getString(it) }
                }.toSet()
        assertEquals(setOf("stroke_replay_erased"), erased)
        assertTrue(batches.none { batch -> batch.strokes.any { it.id in erased } })
    }

    @Test
    fun v1StrokeDecodesAndEncodesBackToTheFixture() {
        val json = fixture("stroke.json")
        val stroke = decodeStroke(json)
        assertEquals(
            v1Stroke("stroke_fixture_1", point(10, 20, 0), point(30, 25, 16), point(50, 40, 32)),
            stroke,
        )
        assertJsonEquals(json, encodeStroke(stroke).toString())
    }

    @Test
    fun pressureStrokeDecodesAndEncodesBackToTheFixture() {
        val json = fixture("stroke-pressure.json")
        val stroke = decodeStroke(json)
        assertEquals(
            Stroke(
                points =
                    listOf(
                        point(10, 20, 0, pressure = 0.0),
                        point(30, 25, 16, pressure = 0.5),
                        point(50, 40, 32, pressure = 1.0),
                        point(70, 55, 48),
                    ),
                id = "stroke_pressure_fixture_1",
                style =
                    StrokeStyle(
                        styleVersion = SOLID_ROUND_PRESSURE_STYLE_VERSION,
                        parameters = SolidRoundParameters(width = 8.0),
                    ),
            ),
            stroke,
        )
        // The trailing point's pressure stays absent on the wire, never `null`.
        assertJsonEquals(json, encodeStroke(stroke).toString())
    }

    // ---- Page channel: client → server ----------------------------------------

    @Test
    fun subscribeEncodesToTheFixture() {
        assertJsonEquals(fixture("page-client-subscribe.json"), encodeSubscribe(0))
    }

    @Test
    fun setPaperEncodesToTheFixture() {
        assertJsonEquals(
            fixture("page-client-set-paper.json"),
            encodeSetPaper("paper_fixture_1", Paper.RuledMarginNarrow),
        )
    }

    @Test
    fun commitBatchEncodesToTheFixture() {
        assertJsonEquals(
            fixture("page-client-commit-batch.json"),
            encodeCommitBatch(
                "batch_cccccccccccccccccccccccc",
                listOf(v1Stroke("stroke_commit_1", point(1, 2, 0), point(3, 4, 10))),
            ),
        )
    }

    /** No fixtures exist for these; their wire types are pinned by the Rust schema gate. */
    @Test
    fun leaseAndTombstoneMessagesEncode() {
        assertJsonEquals(JSONObject("""{"type":"acquire-lease"}"""), encodeAcquireLease())
        assertJsonEquals(JSONObject("""{"type":"renew-lease"}"""), encodeRenewLease())
        assertJsonEquals(JSONObject("""{"type":"release-lease"}"""), encodeReleaseLease())
        assertJsonEquals(
            JSONObject("""{"type":"commit-tombstones","client_mutation_id":"erase_1","stroke_ids":["s1","s2"]}"""),
            encodeCommitTombstones("erase_1", listOf("s1", "s2")),
        )
    }

    // ---- Helpers ---------------------------------------------------------------

    private fun page(paper: Paper): PageSummary =
        PageSummary(
            id = PAGE_ID,
            title = "Meeting notes",
            createdAt = CREATED_AT,
            updatedAt = CREATED_AT,
            thumbnail = ThumbnailMetadata.Empty,
            paper = paper,
        )

    private fun point(
        x: Int,
        y: Int,
        t: Long,
        pressure: Double? = null,
    ): StrokePoint = StrokePoint(x.toDouble(), y.toDouble(), t, pressure)

    private fun v1Stroke(
        id: String,
        vararg points: StrokePoint,
    ): Stroke = Stroke(points = points.toList(), id = id, style = StrokeStyle())

    /** Key order and integer-vs-double spelling are not part of the contract; structure and values are. */
    private fun assertJsonEquals(
        expected: JSONObject,
        actual: String,
    ) {
        assertEquals(plain(expected), plain(JSONObject(actual)))
    }

    private fun plain(value: Any?): Any? =
        when (value) {
            is JSONObject -> value.keys().asSequence().associateWith { plain(value.get(it)) }
            is JSONArray -> (0 until value.length()).map { plain(value.get(it)) }
            is Number -> value.toDouble()
            JSONObject.NULL -> null
            else -> value
        }

    private fun fixture(name: String): JSONObject {
        val file = File(fixturesDir(), name)
        assertTrue("fixture exists: ${file.absolutePath}", file.exists())
        return JSONObject(file.readText())
    }

    private fun fixturesDir(): File = File(File(System.getProperty("user.dir")).parentFile.parentFile, "contracts/fixtures")

    private companion object {
        const val PAGE_ID = "page_01j00000000000000000000000"
        const val CREATED_AT = "2026-06-10T22:00:00Z"
        const val HOLDER = "session_fedcba9876543210"
        const val BATCH_A = "batch_aaaaaaaaaaaaaaaaaaaaaaaa"
        const val BATCH_B = "batch_bbbbbbbbbbbbbbbbbbbbbbbb"

        val ASSERTED_HERE =
            setOf(
                "create-page.json",
                "library-event-page-created.json",
                "library-event-page-deleted.json",
                "library-event-page-thumbnail-updated.json",
                "library-event-page-updated.json",
                "me.json",
                "page-client-commit-batch.json",
                "page-client-set-paper.json",
                "page-client-subscribe.json",
                "page-replay.json",
                "page-server-lease-denied.json",
                "page-server-page-replay.json",
                "page-server-paper-changed.json",
                "page-server-stroke-batch.json",
                "page-server-welcome.json",
                "page.json",
                "pages.json",
                "realtime-ticket.json",
                "stroke-batch.json",
                "stroke-pressure.json",
                "stroke.json",
            )

        // health and meta: FixtureTest. paper-geometry: ink/PaperGeometryTest.
        val ASSERTED_ELSEWHERE = setOf("health.json", "meta.json", "paper-geometry.json")
    }
}
