package link.desync.vnote

import android.graphics.Bitmap
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.toPixelMap
import androidx.compose.ui.test.captureToImage
import androidx.compose.ui.test.junit4.v2.createComposeRule
import androidx.compose.ui.test.onNodeWithTag
import androidx.test.ext.junit.runners.AndroidJUnit4
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.test.UnconfinedTestDispatcher
import kotlinx.coroutines.withContext
import link.desync.vnote.api.ApiClient
import link.desync.vnote.api.LibraryEventListener
import link.desync.vnote.api.PageEventListener
import link.desync.vnote.api.PageSocket
import link.desync.vnote.ink.Paper
import link.desync.vnote.library.PageTile
import link.desync.vnote.model.MeProfile
import link.desync.vnote.model.PageSummary
import link.desync.vnote.model.ThumbnailMetadata
import okhttp3.WebSocket
import org.junit.Rule
import org.junit.Test
import org.junit.rules.RuleChain
import org.junit.rules.TestRule
import org.junit.runner.RunWith
import java.io.ByteArrayOutputStream
import java.util.concurrent.atomic.AtomicInteger

/**
 * A page tile's thumbnail is written on the main thread although its fetch
 * runs on IO (#465).
 *
 * The composition runs on an [UnconfinedTestDispatcher] on purpose: that is
 * the old test harness's dispatcher, which resumes a coroutine on whatever
 * thread woke it. Without the tile's main-thread pin the bitmap state was
 * written, and its snapshot change applied, on the IO worker the fetch
 * finished on — the path that crashed the suite with "Detected multithreaded
 * access to SnapshotStateObserver". [SnapshotThreadGuard] fails the test if
 * that happens; the v2 rule's own dispatcher would hide it.
 */
@RunWith(AndroidJUnit4::class)
class PageTileThreadInstrumentedTest {
    private val composeRule = createComposeRule(UnconfinedTestDispatcher())

    @get:Rule
    val rules: TestRule = RuleChain.outerRule(SnapshotThreadGuard()).around(composeRule)

    private val fetches = AtomicInteger(0)

    @Test
    fun aThumbnailFetchedOnIoIsAppliedOnTheMainThread() {
        val page =
            PageSummary(
                id = PAGE_ID,
                title = "Thumbnail",
                createdAt = "2026-10-19T00:00:00Z",
                updatedAt = "2026-10-19T00:00:00Z",
                thumbnail = ThumbnailMetadata.Available(sourceSeq = 1, url = "/thumbnails/$PAGE_ID.png"),
            )

        composeRule.setContent {
            PageTile(
                apiClient = IoThumbnailClient(fetches),
                page = page,
                thumbnailUnavailable = false,
                onOpen = {},
                onDelete = {},
            )
        }

        // The tile turns red only once the fetched bitmap has been written
        // and drawn, so the write the guard checks has happened by then.
        composeRule.waitUntil(timeoutMillis = 10_000) {
            runCatching {
                val pixels = composeRule.onNodeWithTag("page-tile-$PAGE_ID").captureToImage().toPixelMap()
                pixels[pixels.width / 2, pixels.height / 2] == Color.Red
            }.getOrDefault(false)
        }
        composeRule.waitForIdle()
        check(fetches.get() == 1) { "expected one thumbnail fetch, saw ${fetches.get()}" }
    }

    // Serves a solid red PNG from an IO thread, as OkHttpApiClient does.
    private class IoThumbnailClient(
        private val fetches: AtomicInteger,
    ) : ApiClient {
        override val baseUrl: String = "http://127.0.0.1"

        override suspend fun fetchThumbnail(url: String): Result<ByteArray> =
            withContext(Dispatchers.IO) {
                fetches.incrementAndGet()
                Result.success(redPng())
            }

        override suspend fun checkHealth(): Result<Unit> = unused()

        override suspend fun fetchMe(): Result<MeProfile> = unused()

        override suspend fun listPages(): Result<List<PageSummary>> = unused()

        override suspend fun createPage(
            title: String?,
            paper: Paper,
        ): Result<PageSummary> = unused()

        override suspend fun deletePage(pageId: String): Result<Unit> = unused()

        override fun openLibrarySocket(listener: LibraryEventListener): WebSocket? = unused()

        override fun openPageSocket(
            pageId: String,
            listener: PageEventListener,
        ): PageSocket? = unused()

        override fun shutdown() = Unit

        private fun unused(): Nothing = throw UnsupportedOperationException("not used by a page tile")

        private fun redPng(): ByteArray {
            val bitmap = Bitmap.createBitmap(THUMBNAIL_SIZE, THUMBNAIL_SIZE, Bitmap.Config.ARGB_8888)
            bitmap.eraseColor(android.graphics.Color.RED)
            return ByteArrayOutputStream().use { out ->
                bitmap.compress(Bitmap.CompressFormat.PNG, 100, out)
                out.toByteArray()
            }
        }
    }

    private companion object {
        const val PAGE_ID = "page_thumb"
        const val THUMBNAIL_SIZE = 32
    }
}
