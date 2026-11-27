package link.desync.vnote

import androidx.activity.compose.BackHandler
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.outlined.ArrowBack
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import link.desync.vnote.api.ApiClient
import link.desync.vnote.library.PagePreview
import link.desync.vnote.model.PageSummary
import link.desync.vnote.ui.StatePanel
import link.desync.vnote.ui.StatusBanner
import link.desync.vnote.ui.VNoteTopBar
import link.desync.vnote.ui.displayBinnedAge
import link.desync.vnote.ui.displayTitle

// The recycle bin (#631): pages moved out of the library, each with its
// thumbnail and when it was binned, and the three things to do with them —
// Restore, Delete permanently, and Empty bin. Permanent deletion happens only
// here, and always asks first: it is the one step that destroys ink.

// What a confirmation dialog is asking about.
private sealed interface PendingPurge {
    data class One(
        val page: PageSummary,
    ) : PendingPurge

    // The pages shown when Empty bin was tapped — what the dialog counts and
    // what is deleted, so a page binned elsewhere meanwhile is never included.
    data class All(
        val pages: List<PageSummary>,
    ) : PendingPurge
}

@Composable
internal fun BinScreen(
    apiClient: ApiClient,
    pages: List<PageSummary>,
    error: String?,
    onBack: () -> Unit,
    onRestore: (PageSummary) -> Unit,
    onDeletePermanently: (PageSummary) -> Unit,
    onEmptyBin: (List<PageSummary>) -> Unit,
) {
    BackHandler(onBack = onBack)
    var pending by remember { mutableStateOf<PendingPurge?>(null) }
    pending?.let { purge ->
        PurgeDialog(
            purge = purge,
            onDismiss = { pending = null },
            onConfirm = {
                pending = null
                when (purge) {
                    is PendingPurge.One -> onDeletePermanently(purge.page)
                    is PendingPurge.All -> onEmptyBin(purge.pages)
                }
            },
        )
    }

    Column(modifier = Modifier.fillMaxSize().testTag("bin-screen")) {
        VNoteTopBar {
            IconButton(onClick = onBack) {
                Icon(Icons.AutoMirrored.Outlined.ArrowBack, contentDescription = "Back to library")
            }
            Text(
                "Bin",
                modifier = Modifier.weight(1f),
                style = MaterialTheme.typography.titleLarge,
                maxLines = 1,
            )
            TextButton(
                onClick = { pending = PendingPurge.All(pages) },
                enabled = pages.isNotEmpty(),
                modifier = Modifier.testTag("empty-bin"),
            ) {
                Text("Empty bin")
            }
        }
        BinBody(
            apiClient = apiClient,
            pages = pages,
            error = error,
            onRestore = onRestore,
            onRequestDelete = { pending = PendingPurge.One(it) },
            modifier = Modifier.fillMaxWidth().weight(1f),
        )
    }
}

@Composable
private fun BinBody(
    apiClient: ApiClient,
    pages: List<PageSummary>,
    error: String?,
    onRestore: (PageSummary) -> Unit,
    onRequestDelete: (PageSummary) -> Unit,
    modifier: Modifier = Modifier,
) {
    Column(
        modifier = modifier.padding(16.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        error?.let { StatusBanner(it, isError = true) }
        if (pages.isEmpty()) {
            StatePanel(
                eyebrow = "Bin",
                title = "The bin is empty",
                body = "Pages you delete wait here until you restore them or delete them permanently.",
            )
        } else {
            LazyColumn(verticalArrangement = Arrangement.spacedBy(12.dp)) {
                items(pages, key = { it.id }) { page ->
                    BinRow(
                        apiClient = apiClient,
                        page = page,
                        onRestore = { onRestore(page) },
                        onDeletePermanently = { onRequestDelete(page) },
                    )
                }
            }
        }
    }
}

@Composable
private fun BinRow(
    apiClient: ApiClient,
    page: PageSummary,
    onRestore: () -> Unit,
    onDeletePermanently: () -> Unit,
) {
    Card(
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surface),
        elevation = CardDefaults.cardElevation(defaultElevation = 0.dp),
        border = BorderStroke(1.dp, MaterialTheme.colorScheme.outlineVariant),
        shape = RoundedCornerShape(10.dp),
        modifier = Modifier.fillMaxWidth().testTag("bin-row-${page.id}"),
    ) {
        Row(
            modifier = Modifier.fillMaxWidth().padding(12.dp),
            horizontalArrangement = Arrangement.spacedBy(12.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            PagePreview(
                apiClient = apiClient,
                thumbnail = page.thumbnail,
                unavailable = false,
                modifier = Modifier.width(120.dp).aspectRatio(1.5f).testTag("bin-thumbnail-${page.id}"),
            )
            Column(modifier = Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                Text(
                    page.displayTitle(),
                    style = MaterialTheme.typography.titleMedium,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                Text(
                    page.displayBinnedAge(),
                    modifier = Modifier.testTag("bin-binned-at-${page.id}"),
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.secondary,
                )
                Row(horizontalArrangement = Arrangement.spacedBy(4.dp)) {
                    TextButton(onClick = onRestore, modifier = Modifier.testTag("bin-restore-${page.id}")) {
                        Text("Restore")
                    }
                    TextButton(
                        onClick = onDeletePermanently,
                        modifier = Modifier.testTag("bin-delete-${page.id}"),
                    ) {
                        Text("Delete permanently", color = MaterialTheme.colorScheme.error)
                    }
                }
            }
        }
    }
}

@Composable
private fun PurgeDialog(
    purge: PendingPurge,
    onDismiss: () -> Unit,
    onConfirm: () -> Unit,
) {
    val (title, body) =
        when (purge) {
            is PendingPurge.One ->
                "Delete permanently?" to
                    "${purge.page.displayTitle()} and all its ink will be deleted. This can't be undone."
            is PendingPurge.All -> {
                val what = if (purge.pages.size == 1) "1 page" else "${purge.pages.size} pages"
                "Empty the bin?" to "$what and all their ink will be deleted. This can't be undone."
            }
        }
    AlertDialog(
        onDismissRequest = onDismiss,
        modifier = Modifier.testTag("purge-dialog"),
        title = { Text(title) },
        text = { Text(body) },
        confirmButton = {
            TextButton(onClick = onConfirm, modifier = Modifier.testTag("confirm-purge")) {
                Text("Delete", color = MaterialTheme.colorScheme.error)
            }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}
