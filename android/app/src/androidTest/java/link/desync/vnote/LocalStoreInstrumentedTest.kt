package link.desync.vnote

import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import link.desync.vnote.auth.TokenStore
import link.desync.vnote.ink.Paper
import link.desync.vnote.model.MeProfile
import link.desync.vnote.model.PageSummary
import link.desync.vnote.model.Stroke
import link.desync.vnote.model.StrokePoint
import link.desync.vnote.store.ConfirmedBatch
import link.desync.vnote.store.Edit
import link.desync.vnote.store.LocalStore
import link.desync.vnote.store.OpKind
import link.desync.vnote.store.OpState
import link.desync.vnote.sync.IdentityDecision
import link.desync.vnote.sync.PageRecovery
import link.desync.vnote.sync.decideIdentity
import link.desync.vnote.sync.resetInstallFor
import org.json.JSONArray
import org.json.JSONObject
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import java.time.Instant

/**
 * The device's own copy (#159): what survives a restart, how an
 * acknowledgement lands, what an upgrade does to entries an older version
 * parked as rejected (#651), recovery, and one identity per install.
 * The shared store is emptied before every test by [FreshLocalStore].
 */
@RunWith(AndroidJUnit4::class)
class LocalStoreInstrumentedTest {
    private val context = InstrumentationRegistry.getInstrumentation().targetContext
    private val store = LocalStore.shared(context)
    private val tokenStore = TokenStore(context)

    @After
    fun tearDown() {
        tokenStore.clear()
    }

    /**
     * Acceptance 1: an accepted stroke is on disk the moment it is accepted.
     * A second connection to the same database file — what a restarted
     * process opens — sees it, pending, with its page and paper.
     */
    @Test
    fun anAcceptedEditIsOnDiskForTheNextProcess() {
        store.outbox.enqueue(PAGE, Edit.Batch("batch_1", listOf(stroke("s1"))))
        store.outbox.enqueue(PAGE, Edit.Erase("erase_1", listOf("s0")))
        store.outbox.enqueue(PAGE, Edit.SetPaper("paper_1", Paper.RuledWide))

        val restarted = LocalStore(context, "vnote-local.db")
        val owed = restarted.outbox.read.forPage(PAGE)

        assertEquals(listOf(OpKind.CommitBatch, OpKind.Erase, OpKind.SetPaper), owed.map { it.kind })
        assertTrue("all still pending", owed.all { it.state == OpState.Pending })
        assertEquals("s1", owed[0].strokes.single().id)
        assertEquals(listOf("s0"), owed[1].strokeIds)
        assertEquals(Paper.RuledWide, owed[2].paper)
        assertEquals("in the order they were made", owed.map { it.position }.sorted(), owed.map { it.position })
    }

    /** Acceptance 4: the echo stores the confirmed ink and acknowledges the entry together. */
    @Test
    fun anAcknowledgementPromotesTheBatchToConfirmedInkAtomically() {
        store.outbox.enqueue(PAGE, Edit.Batch("batch_1", listOf(stroke("s1"))))
        store.outbox.markSent("batch_1")

        store.ink.confirmBatch(PAGE, ConfirmedBatch(seq = 7, clientBatchId = "batch_1", strokes = listOf(stroke("s1"))))

        val cached = store.ink.load(PAGE)
        assertEquals(listOf("batch_1"), cached.batches.map { it.clientBatchId })
        assertEquals(7L, cached.lastSeq)
        assertEquals("nothing owed", 0, store.outbox.read.unsyncedCount())
        assertTrue(
            "not listed as owed",
            store.outbox.read
                .forPage(PAGE)
                .isEmpty(),
        )
    }

    /** A repeated echo — redelivery, a replay, a reconnect — stores the batch once. */
    @Test
    fun aRepeatedEchoStoresTheBatchOnce() {
        repeat(3) {
            store.ink.confirmBatch(PAGE, ConfirmedBatch(seq = 3, clientBatchId = "batch_1", strokes = listOf(stroke("s1"))))
        }

        assertEquals(
            1,
            store.ink
                .load(PAGE)
                .batches.size,
        )
    }

    /**
     * #651, acceptance 5: an install the bug left with `rejected` outbox
     * entries comes up with them pending, their refusal's reason cleared.
     * Every entry is still there, once, in the order it was made, with the
     * ink it carried; what was acknowledged or already pending is untouched.
     */
    @Test
    fun upgradingAnInstallPutsRejectedEntriesBackToPending() {
        val name = "vnote-upgrade-test.db"
        context.deleteDatabase(name)
        seedVersionOneStore(
            context,
            name,
            listOf(
                VersionOneOp("batch_done", PAGE, "commit_batch", batchPayload(stroke("done")), "acknowledged"),
                VersionOneOp(
                    "batch_stuck",
                    PAGE,
                    "commit_batch",
                    batchPayload(stroke("s1"), stroke("s2")),
                    "rejected",
                    "forbidden",
                    "HTTP 404",
                ),
                VersionOneOp(
                    "erase_stuck",
                    PAGE,
                    "erase",
                    JSONObject().put("stroke_ids", JSONArray(listOf("s0"))),
                    "rejected",
                    "forbidden",
                    "HTTP 404",
                ),
                VersionOneOp("batch_other", "page_other", "commit_batch", batchPayload(stroke("s3")), "rejected", "page_deleted", "gone"),
                VersionOneOp("batch_sent", PAGE, "commit_batch", batchPayload(stroke("s4")), "sent"),
            ),
        )
        try {
            val upgraded = LocalStore(context, name)

            val owed = upgraded.outbox.read.forPage(PAGE)
            assertEquals("in the order they were made", listOf("batch_stuck", "erase_stuck", "batch_sent"), owed.map { it.opId })
            assertEquals(listOf(OpState.Pending, OpState.Pending, OpState.Sent), owed.map { it.state })
            assertEquals("with their ink", listOf("s1", "s2"), owed[0].strokes.map { it.id })
            assertEquals(listOf("s0"), owed[1].strokeIds)
            assertEquals("every page's", listOf(PAGE, "page_other"), upgraded.outbox.read.pagesWithWork())
            assertEquals("all owed again", 4, upgraded.outbox.read.unsyncedCount())
            upgraded.db.rawQuery("SELECT COUNT(*), COUNT(detail_code), COUNT(detail_message) FROM outbox", null).use { rows ->
                rows.moveToFirst()
                assertEquals("nothing lost, nothing duplicated", 5, rows.getInt(0))
                assertEquals("no refusal reason left", 0, rows.getInt(1) + rows.getInt(2))
            }
        } finally {
            context.deleteDatabase(name)
        }
    }

    /**
     * #651: the strokes owed to a permanently deleted page become a new page
     * made on this device, under fresh ids, owed to the server; the deleted
     * page owes nothing any more.
     */
    @Test
    fun keepingOwedStrokesMakesANewPageUnderFreshIds() {
        store.outbox.enqueue(PAGE, Edit.Batch("batch_1", listOf(stroke("kept"))))
        store.outbox.enqueue(PAGE, Edit.Batch("batch_2", listOf(stroke("erased"))))
        store.outbox.enqueue(PAGE, Edit.Erase("erase_1", listOf("erased")))

        val page = PageRecovery(store).keepAsNewPage(PAGE)

        assertNotEquals(PAGE, page.id)
        assertTrue("made here, not on the server yet", store.library.isLocalOnly(page.id))
        assertTrue(
            "the deleted page owes nothing",
            store.outbox.read
                .forPage(PAGE)
                .isEmpty(),
        )
        val owed = store.outbox.read.forPage(page.id)
        assertEquals(listOf(OpKind.CreatePage, OpKind.CommitBatch), owed.map { it.kind })
        val strokes = owed[1].strokes
        assertEquals("the erased stroke stays erased", 1, strokes.size)
        assertNotEquals("a fresh stroke id", "kept", strokes.single().id)
        assertEquals(stroke("kept").points, strokes.single().points)
    }

    /** The library cache keeps a page made here when the server's list does not have it yet. */
    @Test
    fun theServersListNeverDropsAPageMadeHere() {
        val local = page("page_local")
        store.outbox.createPage(local)

        store.library.write(listOf(page("page_server")))

        assertEquals(
            setOf("page_local", "page_server"),
            store.library
                .pages()
                .map { it.id }
                .toSet(),
        )
    }

    /**
     * #159 review: given the list it replaces, a write touches only what
     * changed — the removed rows go, the changed ones are written, and the
     * rest are left alone (shown by a row the store holds differently from
     * [previous], which an unchanged page must not overwrite).
     */
    @Test
    fun aWriteGivenThePreviousListTouchesOnlyWhatChanged() {
        val kept = page("page_kept")
        val edited = page("page_edited")
        val gone = page("page_gone")
        store.library.write(listOf(kept, edited, gone))
        // The store's row for `kept` drifts from the in-memory list.
        store.library.write(listOf(kept.copy(title = "stored"), edited, gone), previous = listOf(kept, edited, gone))

        val next = listOf(kept, edited.copy(title = "renamed"))
        store.library.write(next, previous = listOf(kept, edited, gone))

        val stored = store.library.pages().associateBy { it.id }
        assertEquals(setOf("page_kept", "page_edited"), stored.keys)
        assertEquals("the changed row is written", "renamed", stored.getValue("page_edited").title)
        assertEquals("an unchanged page is not rewritten", "stored", stored.getValue("page_kept").title)
    }

    /**
     * #657: an edit moves its page's last-updated time to now, in the same
     * transaction as its outbox entry, so the library says so offline. A
     * page whose ink was current stays current — this device's own edit is
     * nothing to fetch — and one already stale stays stale. The server's time
     * wins as soon as the library is written from it, earlier or not.
     */
    @Test
    fun anEditMovesItsPagesLastUpdatedTimeUntilTheServersArrives() {
        val current = page("page_current")
        val stale = page("page_stale")
        store.library.write(listOf(current, stale))
        store.library.markInkSynced(current.id, current.updatedAt)
        val before = Instant.now()

        store.outbox.enqueue(current.id, Edit.Batch("batch_1", listOf(stroke("s1"))))
        store.outbox.enqueue(stale.id, Edit.Erase("erase_1", listOf("s0")))

        val edited = store.library.pages().associateBy { it.id }
        for (id in listOf(current.id, stale.id)) {
            assertTrue("$id was updated now", !Instant.parse(edited.getValue(id).updatedAt).isBefore(before))
        }
        assertEquals(
            "only the page that was already stale needs its ink",
            listOf(stale.id),
            store.library.staleInk().map { it.id },
        )

        val fromServer = current.copy(updatedAt = "2026-11-28T00:00:05Z")
        store.library.write(listOf(fromServer, stale))

        val reconciled = store.library.pages().associateBy { it.id }
        assertEquals("the server's time wins", fromServer.updatedAt, reconciled.getValue(current.id).updatedAt)
        assertEquals("the server's time wins", stale.updatedAt, reconciled.getValue(stale.id).updatedAt)
        assertEquals(
            "a time the server stamped is caught up with once",
            setOf(current.id, stale.id),
            store.library
                .staleInk()
                .map { it.id }
                .toSet(),
        )
    }

    /** #657: an edit never moves a page's last-updated time back, whatever this device's clock says. */
    @Test
    fun anEditNeverMovesTheLastUpdatedTimeBack() {
        val ahead = page("page_ahead").copy(updatedAt = "2999-01-01T00:00:00Z")
        store.library.write(listOf(ahead))
        store.library.markInkSynced(ahead.id, ahead.updatedAt)

        store.outbox.enqueue(ahead.id, Edit.SetPaper("paper_1", Paper.RuledWide))

        val stored = store.library.pages().single()
        assertEquals(ahead.updatedAt, stored.updatedAt)
        assertTrue("still current", store.library.staleInk().isEmpty())
    }

    /** Acceptance 9: signing in again as the same identity keeps the cache and the outbox. */
    @Test
    fun theSameIdentityKeepsEverything() {
        store.identity.save(ME)
        store.library.write(listOf(page("page_server")))
        store.outbox.enqueue(PAGE, Edit.Batch("batch_1", listOf(stroke("s1"))))

        assertEquals(IdentityDecision.Continue, decideIdentity(store, ME.copy(email = "new@example.com")))

        assertEquals(1, store.library.pages().size)
        assertEquals(1, store.outbox.read.unsyncedCount())
    }

    /** Acceptance 9: a different identity with nothing owed resets without asking. */
    @Test
    fun aDifferentIdentityWithNothingOwedResetsSilently() {
        store.identity.save(ME)
        store.library.write(listOf(page("page_server")))

        assertEquals(IdentityDecision.Reset, decideIdentity(store, OTHER))
        resetInstallFor(store, OTHER)

        assertTrue(store.library.pages().isEmpty())
        assertEquals(OTHER.sub, store.identity.current()?.sub)
    }

    /**
     * Acceptance 9: a different identity with changes owed warns first, with
     * the count. Cancel changes nothing — the tokens from before the sign-in
     * come back and every record stays; Delete and continue starts over.
     */
    @Test
    fun aDifferentIdentityWithChangesOwedWarnsAndCancelChangesNothing() {
        store.identity.save(ME)
        store.outbox.enqueue(PAGE, Edit.Batch("batch_1", listOf(stroke("s1"))))
        store.outbox.enqueue(PAGE, Edit.Erase("erase_1", listOf("s0")))
        tokenStore.saveTokens("mine", "my-refresh", 123L)
        val before = tokenStore.snapshot()
        tokenStore.saveTokens("theirs", "their-refresh", 456L)

        assertEquals(IdentityDecision.Warn(unsynced = 2), decideIdentity(store, OTHER))

        // Cancel.
        tokenStore.restore(before)
        assertEquals("mine", tokenStore.accessToken())
        assertEquals("my-refresh", tokenStore.refreshToken())
        assertEquals(ME.sub, store.identity.current()?.sub)
        assertEquals(2, store.outbox.read.unsyncedCount())

        // Delete and continue.
        resetInstallFor(store, OTHER)
        assertEquals(0, store.outbox.read.unsyncedCount())
        assertEquals(OTHER.sub, store.identity.current()?.sub)
    }

    /** An explicit sign-out keeps the identity and everything it owns. */
    @Test
    fun signingOutKeepsTheDevicesCopy() {
        store.identity.save(ME)
        store.outbox.enqueue(PAGE, Edit.Batch("batch_1", listOf(stroke("s1"))))

        store.identity.markSignedOut()

        assertTrue(store.identity.signedOut())
        assertEquals(ME.sub, store.identity.current()?.sub)
        assertEquals(1, store.outbox.read.unsyncedCount())
        store.identity.save(ME)
        assertEquals("signing in again clears the sign-out", false, store.identity.signedOut())
    }

    private fun stroke(id: String) = Stroke(points = listOf(StrokePoint(1.0, 2.0, 0), StrokePoint(3.0, 4.0, 16)), id = id)

    private fun page(id: String) = PageSummary(id = id, title = "", createdAt = "2026-11-28T00:00:00Z", updatedAt = "2026-11-28T00:00:00Z")

    private companion object {
        const val PAGE = "page_store_test"
        val ME = MeProfile(sub = "me", email = "me@example.com")
        val OTHER = MeProfile(sub = "someone-else", email = "else@example.com")
    }
}
