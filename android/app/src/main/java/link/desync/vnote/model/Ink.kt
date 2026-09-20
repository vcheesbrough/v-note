package link.desync.vnote.model

import java.util.UUID

// ---- Canonical ink (world-space strokes) ---------------------------------

// A single stroke sample in world/document coordinates. `t` is milliseconds
// relative to the start of the stroke. `pressure` is an optional normalised
// 0.0..1.0 value; a point with null pressure renders at full width, which is how
// ink migrated up from the retired v1 style keeps its constant-width look.
data class StrokePoint(
    val x: Double,
    val y: Double,
    val t: Long,
    val pressure: Double? = null,
)

// solid_round style discriminator, mirrored from `crates/protocol`. Version 2
// (pressure-modulated) is the only one the server accepts; version 1 was the
// constant-width nib, retired in iteration 48, and the number is never reused.
const val SOLID_ROUND_TOOL = "solid_round"
const val SOLID_ROUND_PRESSURE_STYLE_VERSION = 2

// Shared, cross-platform pressure→width curve constant (see
// `protocol::MIN_PRESSURE_WIDTH`). Absolute rendered nib diameter (world logical
// px) at zero pressure; the curve interpolates linearly from this floor up to
// the preset width, so a heavy pen still tapers to a thin line.
const val MIN_PRESSURE_WIDTH = 1.5

// One captured stroke uses an immutable style snapshot captured at stylus-down.
// The server accepts only the v3 solid_round style, but the discriminated shape
// keeps historical ink unambiguous when future tools arrive.
data class SolidRoundParameters(
    val color: String = "#006400",
    val width: Double = 4.0,
    val capStyle: String = "round",
    val joinStyle: String = "round",
)

data class StrokeStyle(
    val toolKind: String = SOLID_ROUND_TOOL,
    val styleVersion: Int = SOLID_ROUND_PRESSURE_STYLE_VERSION,
    val parameters: SolidRoundParameters = SolidRoundParameters(),
) {
    // Rendered nib diameter for a point carrying the given optional pressure.
    // Must stay identical to `protocol::StrokeStyle::rendered_width`: linear
    // interpolation from an absolute MIN_PRESSURE_WIDTH floor (capped at the
    // preset) up to the preset, treating null as full width.
    fun renderedWidth(pressure: Double?): Double {
        val p = (pressure ?: 1.0).coerceIn(0.0, 1.0)
        val preset = parameters.width
        val floor = minOf(MIN_PRESSURE_WIDTH, preset)
        return floor + (preset - floor) * p
    }
}

data class Stroke(
    val points: List<StrokePoint>,
    val id: String = "stroke_${UUID.randomUUID().toString().replace("-", "")}",
    val style: StrokeStyle = StrokeStyle(),
) {
    companion object {
        const val PEN_WIDTH = 4.0
    }
}
