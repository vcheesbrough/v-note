package link.desync.vnote.ink

import android.content.Context

/**
 * The last paper the user picked, applied as the default for pages they create
 * next. Mirrors [DrawingToolPreferences] structurally.
 *
 * **Known, tracked debt — not a review finding.** The per-page paper itself is
 * fully server-side (`pages.paper`); only this *new-page default* is
 * device-local. `docs/PLAN.md` specifies a user-owned preset, and card #260
 * (widened to cover paper alongside the pen preset) owns building the
 * server-side per-user settings entity, because the repo has no user-scoped
 * server storage at all today. Until then this store is a durable cache, **not
 * the source of truth**.
 *
 * Keep this a thin load/save/validate class with no other responsibilities, so
 * #260 can swap it for a cache over the canonical entity without touching the UI.
 *
 * Uses its own SharedPreferences file so [DrawingToolPreferences.clear] in tests
 * cannot wipe it.
 */
internal class PaperPreferences(
    context: Context,
    private val userId: String,
) {
    private val preferences =
        context.applicationContext.getSharedPreferences(PREFERENCES_NAME, Context.MODE_PRIVATE)

    /** The stored paper, or [Paper.None] when absent or unrecognised. */
    fun load(): Paper = Paper.fromWire(preferences.getString(key(), null)) ?: Paper.None

    fun save(paper: Paper) {
        preferences.edit().putString(key(), paper.wireValue).apply()
    }

    private fun key(): String = "$userId.$PAPER_KEY"

    companion object {
        private const val PREFERENCES_NAME = "page-paper-presets"
        private const val PAPER_KEY = "page-paper"

        internal fun clear(context: Context) {
            context.applicationContext
                .getSharedPreferences(PREFERENCES_NAME, Context.MODE_PRIVATE)
                .edit()
                .clear()
                .commit()
        }
    }
}
