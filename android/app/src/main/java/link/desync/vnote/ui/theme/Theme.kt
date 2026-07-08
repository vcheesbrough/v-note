package link.desync.vnote.ui.theme

import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.Color

private val LightColors =
    lightColorScheme(
        primary = Color(0xFF0B5F35),
        onPrimary = Color.White,
        secondary = Color(0xFF566159),
        background = Color(0xFFF6F7F4),
        surface = Color(0xFFFFFFFF),
        surfaceVariant = Color(0xFFE6E9E2),
        error = Color(0xFF8B1E12),
    )

private val DarkColors =
    darkColorScheme(
        primary = Color(0xFF66B889),
    )

@Composable
fun VNoteTheme(content: @Composable () -> Unit) {
    MaterialTheme(
        colorScheme = LightColors,
        content = content,
    )
}
