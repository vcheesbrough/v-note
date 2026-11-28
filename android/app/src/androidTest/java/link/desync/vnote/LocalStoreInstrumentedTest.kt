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
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith

/**
 * The device's own copy (#159): what survives a restart, how an
 * acknowledgement and a refusal land, recovery, and one identity per install.
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
        assertEquals("nothing owed", 0, store.outbox.read.awaitingCount())
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
     * Acceptance 4, 10: a refusal is kept and never counted as synced; a late
     * echo for a refused entry does not revive it.
     */
    @Test
    fun aRefusalIsKeptAndNeverCountsAsSynced() {
        store.outbox.enqueue(PAGE, Edit.Batch("batch_1", listOf(stroke("s1"), stroke("s2"))))

        store.outbox.reject(PAGE, "page_deleted", "this page was permanently deleted")
        store.outbox.acknowledge("batch_1")

        val rejection =
            store.outbox.read
                .rejections()
                .single()
        assertEquals("page_deleted", rejection.code)
        assertEquals(2, rejection.strokeCount)
        assertEquals("not owed any more", 0, store.outbox.read.awaitingCount())
        assertEquals("but not synced either", 1, store.outbox.read.unsyncedCount())
    }

    /**
     * Acceptance 6: a refused page's strokes become a new page made on this
     * device, under fresh ids, owed to the server; the refusal is gone.
     */
    @Test
    fun keepingRefusedStrokesMakesANewPageUnderFreshIds() {
        store.outbox.enqueue(PAGE, Edit.Batch("batch_1", listOf(stroke("kept"))))
        store.outbox.enqueue(PAGE, Edit.Batch("batch_2", listOf(stroke("erased"))))
        store.outbox.enqueue(PAGE, Edit.Erase("erase_1", listOf("erased")))
        store.outbox.reject(PAGE, "page_deleted", "gone")

        val page = PageRecovery(store).keepAsNewPage(PAGE)

        assertNotEquals(PAGE, page.id)
        assertTrue("made here, not on the server yet", store.library.isLocalOnly(page.id))
        assertTrue(
            "the refusal is settled",
            store.outbox.read
                .rejections()
                .isEmpty(),
        )
        val owed = store.outbox.read.forPage(page.id)
        assertEquals(listOf(OpKind.CreatePage, OpKind.CommitBatch), owed.map { it.kind })
        val strokes = owed[1].strokes
        assertEquals("the erased stroke stays erased", 1, strokes.size)
        assertNotEquals("a fresh stroke id", "kept", strokes.single().id)
        assertEquals(stroke("kept").points, strokes.single().points)
    }

    @Test
    fun discardingARefusalLetsItsEditsGo() {
        store.outbox.enqueue(PAGE, Edit.Batch("batch_1", listOf(stroke("s1"))))
        store.outbox.reject(PAGE, "forbidden", "not yours")

        PageRecovery(store).discard(PAGE)

        assertEquals(0, store.outbox.read.unsyncedCount())
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

    /** Acceptance 9: signing in again as the same identity keeps the cache and the outbox. */
    @Test
    fun theSameIdentityKeepsEverything() {
        store.identity.save(ME)
        store.library.write(listOf(page("page_server")))
        store.outbox.enqueue(PAGE, Edit.Batch("batch_1", listOf(stroke("s1"))))

        assertEquals(IdentityDecision.Continue, decideIdentity(store, ME.copy(email = "new@example.com")))

        assertEquals(1, store.library.pages().size)
        assertEquals(1, store.outbox.read.awaitingCount())
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
        assertEquals(2, store.outbox.read.awaitingCount())

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
        assertEquals(1, store.outbox.read.awaitingCount())
        store.identity.save(ME)
        assertEquals("signing in again clears the sign-out", false, store.identity.signedOut())
        assertNull(
            store.outbox.read
                .rejections()
                .firstOrNull(),
        )
    }

    private fun stroke(id: String) = Stroke(points = listOf(StrokePoint(1.0, 2.0, 0), StrokePoint(3.0, 4.0, 16)), id = id)

    private fun page(id: String) = PageSummary(id = id, title = "", createdAt = "2026-11-28T00:00:00Z", updatedAt = "2026-11-28T00:00:00Z")

    private companion object {
        const val PAGE = "page_store_test"
        val ME = MeProfile(sub = "me", email = "me@example.com")
        val OTHER = MeProfile(sub = "someone-else", email = "else@example.com")
    }
}
