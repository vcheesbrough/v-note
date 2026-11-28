package link.desync.vnote.ink

import androidx.activity.compose.BackHandler
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.outlined.ArrowBack
import androidx.compose.material.icons.outlined.DeleteSweep
import androidx.compose.material3.FilledTonalIconToggleButton
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.semantics.LiveRegionMode
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.liveRegion
import androidx.compose.ui.semantics.selected
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import link.desync.vnote.api.ApiClient
import link.desync.vnote.model.PageSummary
import link.desync.vnote.store.LocalStore
import link.desync.vnote.ui.VNoteTopBar
import link.desync.vnote.ui.displayTitle
import link.desync.vnote.ui.hasDisplayTitle

// The open-page screen: top bar (back, title, paper, pen and eraser controls)
// and the ink canvas, wired to one PageInkSession.

// Full-screen ink editor for one open page: top bar with back + title, and
// the infinite canvas. Connects the page channel on enter and releases it on
// exit. Sync trouble is never shown on the page (#651): whatever fails, the
// page keeps taking ink and the top bar's status icon says what is waiting.
@Composable
fun PageCanvasScreen(
    apiClient: ApiClient,
    page: PageSummary,
    userId: String,
    onBack: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val scope = rememberCoroutineScope()
    val context = LocalContext.current
    val toolPreferences = remember(userId) { DrawingToolPreferences(context, userId) }
    val paperPreferences = remember(userId) { PaperPreferences(context, userId) }
    // The sticky new-page default is written only when the server confirms a
    // change this session asked for — never on dispatch, and never for the
    // paper `Welcome` reports for a page the user merely opened.
    val host = LocalPageSync.current
    val session =
        remember(page.id) {
            PageInkSession(
                apiClient,
                page.id,
                scope,
                page.paper,
                onPaperConfirmed = { confirmed -> paperPreferences.save(confirmed) },
                sync = host?.sessionSync(page.id) ?: SessionSync(LocalStore.shared(context)),
            )
        }
    var selectedTool by remember(page.id) { mutableStateOf(CanvasTool.Drawing) }
    var paletteOpen by remember(page.id) { mutableStateOf(false) }
    // Paper is deliberately *not* a CanvasTool: picking it must never deselect
    // the pen or eraser, so it gets its own open/closed state.
    var paperPaletteOpen by remember(page.id) { mutableStateOf(false) }
    var drawingStyle by remember(userId) { mutableStateOf(toolPreferences.load()) }
    DisposableEffect(page.id) {
        // An open page syncs itself: the engine stands its background session
        // for this page down first.
        host?.engine?.focus(page.id, session)
        session.connect()
        onDispose {
            session.disconnect()
            host?.engine?.focus(null, null)
        }
    }
    // Leaving loses nothing (#159): ink the server has not acknowledged is in
    // the outbox, and the sync engine sends it on.
    val requestLeave = onBack
    BackHandler(onBack = requestLeave)

    Column(modifier = modifier.fillMaxSize()) {
        VNoteTopBar {
            IconButton(onClick = requestLeave) {
                Icon(Icons.AutoMirrored.Outlined.ArrowBack, contentDescription = "Back to library")
            }
            Text(
                page.displayTitle(),
                modifier = Modifier.weight(1f),
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                style = MaterialTheme.typography.titleMedium,
            )
            PaperControl(
                paper = session.paper,
                paletteOpen = paperPaletteOpen,
                // Paper is editable offline too (#159).
                enabled = true,
                onClick = {
                    paperPaletteOpen = !paperPaletteOpen
                    // Opening either palette closes the other.
                    paletteOpen = false
                },
                onPaletteDismiss = { paperPaletteOpen = false },
                onPaperChange = { paper ->
                    session.setPaper(paper)
                    paperPaletteOpen = false
                },
            )
            DrawingToolControl(
                style = drawingStyle,
                selected = selectedTool == CanvasTool.Drawing,
                paletteOpen = paletteOpen,
                onClick = {
                    paperPaletteOpen = false
                    if (selectedTool == CanvasTool.Eraser) {
                        selectedTool = CanvasTool.Drawing
                        paletteOpen = false
                    } else {
                        paletteOpen = !paletteOpen
                    }
                },
                onPaletteDismiss = { paletteOpen = false },
                onStyleChange = { style ->
                    drawingStyle = style
                    toolPreferences.save(style)
                },
            )
            FilledTonalIconToggleButton(
                checked = selectedTool == CanvasTool.Eraser,
                onCheckedChange = {
                    selectedTool = CanvasTool.Eraser
                    paletteOpen = false
                    paperPaletteOpen = false
                },
                modifier =
                    Modifier
                        .size(48.dp)
                        .testTag("eraser-tool")
                        .semantics { selected = selectedTool == CanvasTool.Eraser },
            ) {
                Icon(Icons.Outlined.DeleteSweep, contentDescription = "Eraser")
            }
        }
        session.binnedAt?.let {
            BinnedPageNotice(page = page, restoreError = session.restoreError, onRestore = session::restore)
        }
        InkCanvas(
            strokes = session.strokes,
            paper = session.paper,
            selectedTool = selectedTool,
            drawingStyle = drawingStyle,
            onStrokeFinished = session::commitStroke,
            onStrokesErased = session::eraseStrokes,
            modifier = Modifier.fillMaxWidth().weight(1f),
        )
    }
}

// Shown while the open page is in the bin (#631) — binned on another device,
// or opened after it was. Not an error and not a block: ink drawn here is
// still saved, it just stays in the bin with the page.
@Composable
private fun BinnedPageNotice(
    page: PageSummary,
    restoreError: String?,
    onRestore: () -> Unit,
) {
    // An untitled page's label is its age, which reads badly mid-sentence.
    val which = if (page.hasDisplayTitle()) "Page \u201c${page.title}\u201d" else "This page"
    Surface(
        color = MaterialTheme.colorScheme.tertiaryContainer,
        shape = RoundedCornerShape(7.dp),
        modifier =
            Modifier
                .fillMaxWidth()
                .padding(horizontal = 12.dp, vertical = 4.dp)
                .testTag("page-binned-notice")
                .semantics { liveRegion = LiveRegionMode.Polite },
    ) {
        Row(
            modifier = Modifier.padding(start = 12.dp, end = 4.dp, top = 6.dp, bottom = 6.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Column(modifier = Modifier.weight(1f)) {
                Text(
                    "$which was moved to the bin on another device; your changes are kept there.",
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onTertiaryContainer,
                )
                // A failed Restore says so; tapping it again retries.
                restoreError?.let { error ->
                    Text(
                        "Couldn't restore it: $error",
                        modifier = Modifier.testTag("page-binned-restore-error"),
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.error,
                    )
                }
            }
            TextButton(onClick = onRestore, modifier = Modifier.testTag("page-binned-restore")) { Text("Restore") }
        }
    }
}
