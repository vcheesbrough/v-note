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
import androidx.compose.material3.AlertDialog
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
import link.desync.vnote.ui.VNoteTopBar
import link.desync.vnote.ui.displayTitle
import link.desync.vnote.ui.hasDisplayTitle

// The open-page screen: top bar (back, title, paper, pen and eraser controls), the
// status banner, and the ink canvas, wired to one PageInkSession.

// Full-screen ink editor for one open page: top bar with back + title, the
// sync status banner when there is trouble (#150: disconnected, retrying,
// another editor, session expired, rejected), and the infinite canvas.
// Connects the page channel on enter and releases it on exit. [onSignIn] is the
// banner's action when the session expired and a refresh could not renew it:
// leave the page and sign in again.
@Composable
fun PageCanvasScreen(
    apiClient: ApiClient,
    page: PageSummary,
    userId: String,
    onBack: () -> Unit,
    modifier: Modifier = Modifier,
    onSignIn: () -> Unit = onBack,
) {
    val scope = rememberCoroutineScope()
    val context = LocalContext.current
    val toolPreferences = remember(userId) { DrawingToolPreferences(context, userId) }
    val paperPreferences = remember(userId) { PaperPreferences(context, userId) }
    // The sticky new-page default is written only when the server confirms a
    // change this session asked for — never on dispatch, and never for the
    // paper `Welcome` reports for a page the user merely opened.
    val session =
        remember(page.id) {
            PageInkSession(apiClient, page.id, scope, page.paper) { confirmed ->
                paperPreferences.save(confirmed)
            }
        }
    var selectedTool by remember(page.id) { mutableStateOf(CanvasTool.Drawing) }
    var paletteOpen by remember(page.id) { mutableStateOf(false) }
    // Paper is deliberately *not* a CanvasTool: picking it must never deselect
    // the pen or eraser, so it gets its own open/closed state.
    var paperPaletteOpen by remember(page.id) { mutableStateOf(false) }
    var drawingStyle by remember(userId) { mutableStateOf(toolPreferences.load()) }
    DisposableEffect(page.id) {
        session.connect()
        onDispose { session.disconnect() }
    }
    // Leaving drops ink the server has not acknowledged — held while the
    // connection was missing or catching up (#390), or sent and not yet echoed — so
    // ask first. With nothing unsent, back leaves at once, as it always did.
    // Signing in again (#150) leaves the page too, so it asks the same way.
    var unsentOnLeave by remember(page.id) { mutableStateOf<Pair<Int, () -> Unit>?>(null) }
    val leaveThen = { leave: () -> Unit ->
        val unsent = session.unsentStrokeCount
        if (unsent > 0) unsentOnLeave = unsent to leave else leave()
    }
    val requestLeave = { leaveThen(onBack) }
    val requestSignIn = { leaveThen(onSignIn) }
    BackHandler(onBack = requestLeave)
    unsentOnLeave?.let { (unsent, leave) ->
        UnsentInkDialog(
            strokes = unsent,
            onStay = { unsentOnLeave = null },
            onLeave = {
                unsentOnLeave = null
                leave()
            },
        )
    }

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
                enabled = session.canEdit,
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
            BinnedPageNotice(page = page, onRestore = session::restore)
        }
        session.status?.let { status ->
            PageStatusBanner(
                status = status,
                unsentStrokes = session.unsentStrokeCount,
                onRetry = session::retryNow,
                onSignIn = requestSignIn,
            )
        }
        InkCanvas(
            strokes = session.strokes,
            paper = session.paper,
            inkBlocked = session.inkBlocked,
            selectedTool = selectedTool,
            drawingStyle = drawingStyle,
            onStrokeFinished = session::commitStroke,
            onStrokesErased = session::eraseStrokes,
            modifier = Modifier.fillMaxWidth().weight(1f),
        )
    }
}

@Composable
private fun UnsentInkDialog(
    strokes: Int,
    onStay: () -> Unit,
    onLeave: () -> Unit,
) {
    AlertDialog(
        onDismissRequest = onStay,
        modifier = Modifier.testTag("unsent-ink-dialog"),
        title = { Text("Unsent ink") },
        text = {
            val what = if (strokes == 1) "1 stroke hasn't" else "$strokes strokes haven't"
            Text("$what reached the server and will be lost if you leave.")
        },
        confirmButton = {
            TextButton(onClick = onStay, modifier = Modifier.testTag("unsent-ink-stay")) { Text("Stay") }
        },
        dismissButton = {
            TextButton(onClick = onLeave, modifier = Modifier.testTag("unsent-ink-leave")) { Text("Leave anyway") }
        },
    )
}

// Shown while the open page is in the bin (#631) — binned on another device,
// or opened after it was. Not an error and not a block: ink drawn here is
// still saved, it just stays in the bin with the page.
@Composable
private fun BinnedPageNotice(
    page: PageSummary,
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
            Text(
                "$which was moved to the bin on another device; your changes are kept there.",
                modifier = Modifier.weight(1f),
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onTertiaryContainer,
            )
            TextButton(onClick = onRestore, modifier = Modifier.testTag("page-binned-restore")) { Text("Restore") }
        }
    }
}
