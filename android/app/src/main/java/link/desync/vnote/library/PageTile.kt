package link.desync.vnote.library

import android.graphics.BitmapFactory
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.Image
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.produceState
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import link.desync.vnote.api.ApiClient
import link.desync.vnote.model.PageSummary
import link.desync.vnote.model.ThumbnailMetadata
import link.desync.vnote.ui.displayUpdatedAge
import link.desync.vnote.ui.hasDisplayTitle

// One page in the library grid: the thumbnail that opens it, its age, and
// delete. The SPA counterpart is `.page-tile`.

private val PagePreviewRuleColor = Color(0xFFEAE5DA)
private val PagePreviewEdgeColor = Color(0xFFE0DBCD)

@Composable
internal fun PageTile(
    apiClient: ApiClient,
    page: PageSummary,
    thumbnailUnavailable: Boolean,
    onOpen: () -> Unit,
    onDelete: () -> Unit,
) {
    Card(
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surface),
        elevation = CardDefaults.cardElevation(defaultElevation = 0.dp),
        border = BorderStroke(1.dp, MaterialTheme.colorScheme.outlineVariant),
        shape = RoundedCornerShape(10.dp),
        modifier = Modifier.fillMaxWidth(),
    ) {
        Column(modifier = Modifier.fillMaxWidth()) {
            Box(
                modifier =
                    Modifier
                        .fillMaxWidth()
                        .aspectRatio(1.5f)
                        .testTag("page-tile-${page.id}")
                        .clickable(onClick = onOpen),
            ) {
                PagePreview(
                    apiClient = apiClient,
                    thumbnail = page.thumbnail,
                    unavailable = thumbnailUnavailable,
                    modifier = Modifier.fillMaxSize(),
                )
                if (page.hasDisplayTitle()) {
                    PageTitleChip(title = page.title, modifier = Modifier.align(Alignment.TopStart))
                }
            }
            PageTileFooter(page = page, onDelete = onDelete)
        }
    }
}

@Composable
private fun PageTitleChip(
    title: String,
    modifier: Modifier = Modifier,
) {
    Surface(
        color = Color.White.copy(alpha = 0.92f),
        shape = RoundedCornerShape(4.dp),
        modifier = modifier.padding(8.dp).widthIn(max = 180.dp),
    ) {
        Text(
            title,
            modifier = Modifier.padding(horizontal = 8.dp, vertical = 4.dp),
            style = MaterialTheme.typography.bodyMedium,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
        )
    }
}

@Composable
private fun PageTileFooter(
    page: PageSummary,
    onDelete: () -> Unit,
) {
    Row(
        modifier = Modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 10.dp),
        horizontalArrangement = Arrangement.spacedBy(8.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(
            page.displayUpdatedAge(),
            modifier = Modifier.weight(1f),
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.secondary,
        )
        IconButton(
            modifier =
                Modifier
                    .size(32.dp)
                    .semantics {
                        contentDescription = if (page.hasDisplayTitle()) "Delete ${page.title}" else "Delete page"
                    },
            onClick = onDelete,
        ) {
            Text(
                "×",
                style = MaterialTheme.typography.titleMedium,
                color = MaterialTheme.colorScheme.error,
            )
        }
    }
}

@Composable
private fun PagePreview(
    apiClient: ApiClient,
    thumbnail: ThumbnailMetadata,
    unavailable: Boolean,
    modifier: Modifier = Modifier,
) {
    val bitmap by produceState<android.graphics.Bitmap?>(initialValue = null, key1 = thumbnail, key2 = unavailable) {
        value =
            if (!unavailable && thumbnail is ThumbnailMetadata.Available) {
                apiClient.fetchThumbnail(thumbnail.url).getOrNull()?.let { bytes ->
                    BitmapFactory.decodeByteArray(bytes, 0, bytes.size)
                }
            } else {
                null
            }
    }
    val failedColor = MaterialTheme.colorScheme.errorContainer
    Box(
        modifier =
            modifier
                .drawBehind {
                    drawRect(if (unavailable || thumbnail is ThumbnailMetadata.Failed) failedColor else Color.White)
                    val step = 8.dp.toPx()
                    var y = step
                    while (y < size.height) {
                        drawLine(
                            color = PagePreviewRuleColor,
                            start = Offset(6.dp.toPx(), y),
                            end = Offset(size.width - 6.dp.toPx(), y),
                            strokeWidth = 1.dp.toPx(),
                        )
                        y += step
                    }
                    drawLine(
                        color = PagePreviewEdgeColor,
                        start = Offset(0f, size.height),
                        end = Offset(size.width, size.height),
                        strokeWidth = 1.dp.toPx(),
                    )
                },
        contentAlignment = Alignment.Center,
    ) {
        if (bitmap != null) {
            Image(bitmap = bitmap!!.asImageBitmap(), contentDescription = null, modifier = Modifier.fillMaxSize())
        } else if (thumbnail is ThumbnailMetadata.Generating) {
            Text("...", style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.secondary)
        }
    }
}
