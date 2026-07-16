package link.desync.vnote.ink

import android.content.Context
import link.desync.vnote.auth.SolidRoundParameters
import link.desync.vnote.auth.StrokeStyle
import kotlin.math.roundToInt

internal class DrawingToolPreferences(
    context: Context,
    private val userId: String,
) {
    private val preferences =
        context.applicationContext.getSharedPreferences(PREFERENCES_NAME, Context.MODE_PRIVATE)

    fun load(): StrokeStyle {
        val color = preferences.getString(key(COLOR_KEY), null)
        val width = preferences.getString(key(WIDTH_KEY), null)?.toDoubleOrNull()
        if (!isCanonicalColor(color) || !isValidWidth(width)) {
            return StrokeStyle()
        }
        return StrokeStyle(
            parameters =
                SolidRoundParameters(
                    color = color!!,
                    width = width!!,
                ),
        )
    }

    fun save(style: StrokeStyle) {
        require(style.toolKind == "solid_round" && style.styleVersion == 1)
        require(isCanonicalColor(style.parameters.color))
        require(isValidWidth(style.parameters.width))
        preferences
            .edit()
            .putString(key(COLOR_KEY), style.parameters.color)
            .putString(key(WIDTH_KEY), style.parameters.width.toString())
            .apply()
    }

    private fun key(name: String): String = "$userId.$name"

    companion object {
        private const val PREFERENCES_NAME = "drawing-tool-presets"
        private const val COLOR_KEY = "solid-round-v1-color"
        private const val WIDTH_KEY = "solid-round-v1-width"
        private val COLOR_PATTERN = Regex("#[0-9A-F]{6}")

        internal fun clear(context: Context) {
            context.applicationContext
                .getSharedPreferences(PREFERENCES_NAME, Context.MODE_PRIVATE)
                .edit()
                .clear()
                .commit()
        }

        private fun isCanonicalColor(color: String?): Boolean =
            color != null && COLOR_PATTERN.matches(color)

        private fun isValidWidth(width: Double?): Boolean =
            width != null &&
                width.isFinite() &&
                width in 1.0..32.0 &&
                (width * 2.0).roundToInt().toDouble() == width * 2.0
    }
}
