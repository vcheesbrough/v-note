package link.desync.vnote.ui.theme

import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.Color

// Ink on paper: a warm paper ground, near-black ink and one fountain-pen blue
// accent. `frontend/src/styles.css` carries the same values in its `:root`
// custom properties, so the two clients read as one product — change them
// together. Light only; the app has no dark theme yet.

private val LightColors =
    lightColorScheme(
        primary = Color(0xFF1F3A6E),
        onPrimary = Color(0xFFFFFFFF),
        primaryContainer = Color(0xFFDDE4F1),
        onPrimaryContainer = Color(0xFF16294F),
        secondary = Color(0xFF6C6960),
        onSecondary = Color(0xFFFFFFFF),
        // The selected-tool tint for `FilledTonalIconToggleButton` (pen, eraser,
        // paper). It must stay clearly apart from `surfaceVariant`, which is the
        // same control's unselected ground.
        secondaryContainer = Color(0xFFD5E0F2),
        onSecondaryContainer = Color(0xFF16294F),
        background = Color(0xFFF2EFE8),
        onBackground = Color(0xFF1D2026),
        surface = Color(0xFFFFFFFF),
        onSurface = Color(0xFF1D2026),
        surfaceVariant = Color(0xFFE9E4D9),
        onSurfaceVariant = Color(0xFF4C4941),
        outline = Color(0xFFC9C2B0),
        outlineVariant = Color(0xFFE0DBCD),
        error = Color(0xFF98291F),
        onError = Color(0xFFFFFFFF),
        errorContainer = Color(0xFFF6E5E1),
        onErrorContainer = Color(0xFF5C1710),
    )

@Composable
fun VNoteTheme(content: @Composable () -> Unit) {
    MaterialTheme(
        colorScheme = LightColors,
        content = content,
    )
}
