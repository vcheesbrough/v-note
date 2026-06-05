package link.desync.vnote.ui.theme

import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable

private val LightColors =
    lightColorScheme(
        primary = androidx.compose.ui.graphics.Color(0xFF006400),
    )

private val DarkColors =
    darkColorScheme(
        primary = androidx.compose.ui.graphics.Color(0xFF006400),
    )

@Composable
fun VNoteTheme(content: @Composable () -> Unit) {
    MaterialTheme(
        colorScheme = LightColors,
        content = content,
    )
}
