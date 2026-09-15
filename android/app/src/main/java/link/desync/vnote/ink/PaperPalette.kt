package link.desync.vnote.ink

import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.FilledTonalIconToggleButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.selected
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.dp

// The paper control and its palette of paper choices, each drawn as a live swatch.

/**
 * Top-bar paper picker, structurally the twin of [DrawingToolControl]: a toggle
 * button whose icon is a live swatch of the current paper, plus an anchored
 * dropdown palette of all seven choices.
 *
 * Paper is **not** a [CanvasTool] — `checked` binds to the palette's own open
 * state and never touches `selectedTool`, so picking paper leaves the pen or
 * eraser exactly as it was. Disabled without the edit lease, which the existing
 * lease banner already explains.
 */
@Composable
internal fun PaperControl(
    paper: Paper,
    paletteOpen: Boolean,
    enabled: Boolean,
    onClick: () -> Unit,
    onPaletteDismiss: () -> Unit,
    onPaperChange: (Paper) -> Unit,
) {
    Box {
        FilledTonalIconToggleButton(
            checked = paletteOpen,
            onCheckedChange = { onClick() },
            enabled = enabled,
            modifier =
                Modifier
                    .size(48.dp)
                    .testTag("paper-tool")
                    .semantics {
                        this.selected = paletteOpen
                        contentDescription = "Paper"
                    },
        ) {
            PaperPreview(paper = paper, modifier = Modifier.size(width = 32.dp, height = 24.dp))
        }
        DropdownMenu(
            expanded = paletteOpen,
            onDismissRequest = onPaletteDismiss,
            modifier = Modifier.width(224.dp).testTag("paper-palette"),
        ) {
            Text(
                "Paper",
                modifier = Modifier.padding(horizontal = 12.dp, vertical = 4.dp),
                style = MaterialTheme.typography.labelLarge,
            )
            Paper.ALL.forEach { option ->
                PaperOption(
                    paper = option,
                    selected = option == paper,
                    onClick = { onPaperChange(option) },
                )
            }
        }
    }
}

@Composable
private fun PaperOption(
    paper: Paper,
    selected: Boolean,
    onClick: () -> Unit,
) {
    Row(
        modifier =
            Modifier
                .fillMaxWidth()
                .testTag("paper-option-${paper.wireValue}")
                .semantics {
                    this.selected = selected
                    contentDescription = paper.label
                }.clickable(onClick = onClick)
                .padding(horizontal = 12.dp, vertical = 6.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        PaperPreview(
            paper = paper,
            modifier =
                Modifier
                    .size(width = 40.dp, height = 30.dp)
                    .then(
                        if (selected) {
                            Modifier.border(3.dp, MaterialTheme.colorScheme.onSurface)
                        } else {
                            Modifier.border(1.dp, MaterialTheme.colorScheme.outline)
                        },
                    ),
        )
        Text(paper.label, style = MaterialTheme.typography.bodyMedium)
    }
}

/**
 * A live swatch of [paper] on a white ground. Uses the shared
 * [previewViewport], which raises the scale to clear the density cull —
 * otherwise a 32×24.dp swatch of 48-unit rules is culled and the icon renders
 * blank.
 */
@Composable
private fun PaperPreview(
    paper: Paper,
    modifier: Modifier,
) {
    Box(modifier = modifier.background(Color.White)) {
        Canvas(modifier = Modifier.fillMaxSize()) {
            drawPaperPreview(paper, size)
        }
    }
}
