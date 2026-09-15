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
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.FilledTonalIconToggleButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Slider
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.selected
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.dp
import link.desync.vnote.model.StrokeStyle
import java.util.Locale
import kotlin.math.roundToInt

// The pen control and its palette: colour swatches, width slider and a live
// stroke preview.

private const val MIN_WIDTH = 1f
private const val MAX_WIDTH = 32f
private const val WIDTH_INCREMENT = 0.5f
private val ToolColors =
    listOf(
        "#000000",
        "#4B5563",
        "#006400",
        "#00796B",
        "#1565C0",
        "#6A1B9A",
        "#C62828",
        "#EF6C00",
    )

@Composable
internal fun DrawingToolControl(
    style: StrokeStyle,
    selected: Boolean,
    paletteOpen: Boolean,
    onClick: () -> Unit,
    onPaletteDismiss: () -> Unit,
    onStyleChange: (StrokeStyle) -> Unit,
) {
    Box {
        FilledTonalIconToggleButton(
            checked = selected,
            onCheckedChange = { onClick() },
            modifier =
                Modifier
                    .size(48.dp)
                    .testTag("drawing-tool")
                    .semantics {
                        this.selected = selected
                        contentDescription = "Drawing tool"
                    },
        ) {
            ToolStrokePreview(style = style, modifier = Modifier.size(width = 32.dp, height = 24.dp))
        }
        DropdownMenu(
            expanded = paletteOpen,
            onDismissRequest = onPaletteDismiss,
            modifier = Modifier.width(224.dp).testTag("tool-palette"),
        ) {
            Text(
                "Colour",
                modifier = Modifier.padding(horizontal = 12.dp, vertical = 4.dp),
                style = MaterialTheme.typography.labelLarge,
            )
            ToolColors.chunked(4).forEach { rowColors ->
                Row(modifier = Modifier.padding(horizontal = 4.dp)) {
                    rowColors.forEach { color ->
                        ColorSwatch(
                            color = color,
                            selected = style.parameters.color == color,
                            onClick = {
                                onStyleChange(
                                    style.copy(parameters = style.parameters.copy(color = color)),
                                )
                            },
                        )
                    }
                }
            }
            Row(
                modifier = Modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 4.dp),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                ToolStrokePreview(
                    style = style,
                    modifier = Modifier.size(width = 72.dp, height = 32.dp),
                )
                Text(
                    String.format(Locale.US, "%.1f", style.parameters.width),
                    modifier = Modifier.testTag("tool-width-value"),
                    style = MaterialTheme.typography.labelLarge,
                )
            }
            Slider(
                value = style.parameters.width.toFloat(),
                onValueChange = { raw ->
                    val stepped =
                        (raw / WIDTH_INCREMENT).roundToInt() * WIDTH_INCREMENT
                    onStyleChange(
                        style.copy(parameters = style.parameters.copy(width = stepped.toDouble())),
                    )
                },
                valueRange = MIN_WIDTH..MAX_WIDTH,
                steps = 61,
                modifier =
                    Modifier
                        .fillMaxWidth()
                        .padding(horizontal = 12.dp)
                        .testTag("tool-width-slider")
                        .semantics { contentDescription = "Stroke width" },
            )
        }
    }
}

@Composable
private fun ColorSwatch(
    color: String,
    selected: Boolean,
    onClick: () -> Unit,
) {
    Box(
        modifier =
            Modifier
                .size(48.dp)
                .testTag("swatch-$color")
                .semantics {
                    this.selected = selected
                    contentDescription = "Colour $color"
                }.clickable(onClick = onClick),
        contentAlignment = Alignment.Center,
    ) {
        Box(
            modifier =
                Modifier
                    .size(32.dp)
                    .background(parseColor(color), CircleShape)
                    .then(
                        if (selected) {
                            Modifier.border(3.dp, MaterialTheme.colorScheme.onSurface, CircleShape)
                        } else {
                            Modifier.border(1.dp, MaterialTheme.colorScheme.outline, CircleShape)
                        },
                    ),
        )
    }
}

@Composable
private fun ToolStrokePreview(
    style: StrokeStyle,
    modifier: Modifier,
) {
    Surface(modifier = modifier) {
        Canvas(modifier = Modifier.fillMaxSize().padding(2.dp)) {
            val previewWidth =
                style.parameters.width
                    .toFloat()
                    .coerceIn(2f, size.height - 2f)
            drawLine(
                color = parseColor(style.parameters.color),
                start = Offset(2f, size.height / 2f),
                end = Offset(size.width - 2f, size.height / 2f),
                strokeWidth = previewWidth,
                cap = StrokeCap.Round,
            )
        }
    }
}

internal fun parseColor(value: String): Color = Color(android.graphics.Color.parseColor(value))
