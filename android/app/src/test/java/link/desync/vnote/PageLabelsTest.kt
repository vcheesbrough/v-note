package link.desync.vnote

import link.desync.vnote.model.PageSummary
import link.desync.vnote.ui.displayBinnedAge
import link.desync.vnote.ui.displayTitle
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Test

class PageLabelsTest {
    @Test
    fun untitledPageWithOffsetTimestampUsesRelativeUpdatedLabel() {
        val page =
            PageSummary(
                id = "page-1",
                title = "",
                createdAt = "2026-07-06T21:45:00+00:00",
                updatedAt = "2026-07-06T21:45:00+00:00",
            )

        assertFalse(page.displayTitle().startsWith("2026-07-06 21:45"))
    }

    @Test
    fun binnedPageSaysWhenItWentToTheBin() {
        val page =
            PageSummary(
                id = "page-1",
                title = "",
                createdAt = "2026-07-06T21:45:00+00:00",
                updatedAt = "2026-07-06T21:45:00+00:00",
                binnedAt =
                    java.time.Instant
                        .now()
                        .toString(),
            )

        assertEquals("Moved to the bin just now", page.displayBinnedAge())
    }
}
