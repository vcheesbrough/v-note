package link.desync.vnote.ink

import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.outlined.ArrowBack
import androidx.compose.material.icons.outlined.DeleteSweep
import androidx.compose.material3.FilledTonalIconToggleButton
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.selected
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import link.desync.vnote.api.ApiClient
import link.desync.vnote.model.PageSummary
import link.desync.vnote.ui.VNoteTopBar
import link.desync.vnote.ui.displayTitle

// The open-page screen: top bar (back, title, paper, pen and eraser controls), the
// status banner, and the ink canvas, wired to one PageInkSession.

// Full-screen ink editor for one open page: top bar with back + title, an
// optional status banner (e.g. blocked-by-another-editor), and the infinite
// canvas. Connects the page channel on enter and releases it on exit.
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

    Column(modifier = modifier.fillMaxSize()) {
        VNoteTopBar {
            IconButton(onClick = onBack) {
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
        session.statusBanner?.let { banner ->
            Text(
                banner,
                modifier = Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 4.dp),
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.error,
            )
        }
        InkCanvas(
            strokes = session.strokes,
            paper = session.paper,
            canEdit = session.canEdit,
            selectedTool = selectedTool,
            drawingStyle = drawingStyle,
            onStrokeFinished = session::commitStroke,
            onStrokesErased = session::eraseStrokes,
            modifier = Modifier.fillMaxWidth().weight(1f),
        )
    }
}
