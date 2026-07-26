package link.desync.vnote.ink

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.File

/**
 * The cross-language lock for page paper.
 *
 * `contracts/fixtures/paper-geometry.json` is generated from `protocol::paper`
 * and asserted from both sides — here and in
 * `crates/protocol/tests/contracts.rs`. If the Kotlin mirror in [Paper] drifts
 * from the Rust spec by so much as one pitch, colour, cull threshold or mark
 * position, this test fails. Parity is proved, not eyeballed.
 *
 * The analogue of `PressureWidthTest` for the pressure→width curve.
 */
class PaperGeometryTest {
    private val fixture: JSONObject by lazy {
        val root = File(System.getProperty("user.dir")).parentFile.parentFile
        val file = File(root, "contracts/fixtures/paper-geometry.json")
        assertTrue("fixture exists: ${file.absolutePath}", file.exists())
        JSONObject(file.readText())
    }

    @Test
    fun constantsMatchTheSharedSpec() {
        val constants = fixture.getJSONObject("constants")
        assertEquals(constants.getDouble("rule_spacing_narrow"), RULE_SPACING_NARROW, 0.0)
        assertEquals(constants.getDouble("rule_spacing_wide"), RULE_SPACING_WIDE, 0.0)
        assertEquals(constants.getDouble("grid_spacing_small"), GRID_SPACING_SMALL, 0.0)
        assertEquals(constants.getDouble("grid_spacing_large"), GRID_SPACING_LARGE, 0.0)
        assertEquals(constants.getDouble("margin_x"), MARGIN_X, 0.0)
        assertEquals(constants.getDouble("rule_line_width"), RULE_LINE_WIDTH, 0.0)
        assertEquals(constants.getDouble("margin_line_width"), MARGIN_LINE_WIDTH, 0.0)
        assertEquals(constants.getString("rule_color"), RULE_COLOR)
        assertEquals(constants.getString("margin_color"), MARGIN_COLOR)
        assertEquals(
            constants.getDouble("min_paper_mark_device_pitch"),
            MIN_PAPER_MARK_DEVICE_PITCH,
            0.0,
        )
        assertEquals(
            constants.getDouble("min_paper_mark_device_width"),
            MIN_PAPER_MARK_DEVICE_WIDTH,
            0.0,
        )
        assertEquals(constants.getInt("max_paper_marks_per_axis"), MAX_PAPER_MARKS_PER_AXIS)
    }

    @Test
    fun paletteMatchesTheSharedSpecInOrder() {
        val papers = fixture.getJSONArray("papers")
        assertEquals("palette size", papers.length(), Paper.ALL.size)
        for (index in 0 until papers.length()) {
            val expected = papers.getJSONObject(index)
            val actual = Paper.ALL[index]
            assertEquals("wire value at $index", expected.getString("wire_value"), actual.wireValue)
            assertEquals("label of ${actual.wireValue}", expected.getString("label"), actual.label)
            assertEquals(
                "has_margin of ${actual.wireValue}",
                expected.getBoolean("has_margin"),
                actual.hasMargin,
            )
            assertEquals(
                "rule_spacing of ${actual.wireValue}",
                expected.optDouble("rule_spacing").takeIf { !it.isNaN() },
                actual.ruleSpacing,
            )
            assertEquals(
                "column_spacing of ${actual.wireValue}",
                expected.optDouble("column_spacing").takeIf { !it.isNaN() },
                actual.columnSpacing,
            )
        }
        assertEquals("None leads the palette", Paper.None, Paper.ALL.first())
    }

    @Test
    fun markEnumerationMatchesTheSharedSpec() {
        val cases = fixture.getJSONArray("cases")
        assertTrue("golden pins cases", cases.length() > 0)
        var totalMarks = 0
        for (index in 0 until cases.length()) {
            val case = cases.getJSONObject(index)
            val name = case.getString("name")
            val paper = Paper.fromWire(case.getString("paper"))
            assertNotNull("known paper in case $name", paper)
            val viewportJson = case.getJSONObject("viewport")
            val viewport =
                PaperViewport(
                    minX = viewportJson.getDouble("min_x"),
                    minY = viewportJson.getDouble("min_y"),
                    maxX = viewportJson.getDouble("max_x"),
                    maxY = viewportJson.getDouble("max_y"),
                    scale = viewportJson.getDouble("scale"),
                )
            val expected = case.getJSONArray("marks")
            val actual = paperMarks(paper!!, viewport)
            assertEquals("mark count in case $name", expected.length(), actual.size)
            totalMarks += actual.size
            for (markIndex in 0 until expected.length()) {
                val want = expected.getJSONObject(markIndex)
                val got = actual[markIndex]
                val where = "$name mark $markIndex"
                assertEquals("$where kind", want.getString("kind"), got.kind.wireKind)
                assertEquals("$where position", want.getDouble("position"), got.position, 0.0)
                assertEquals("$where width", want.getDouble("world_width"), got.worldWidth, 0.0)
                assertEquals("$where color", want.getString("color"), got.kind.color)
            }
        }
        assertTrue("golden pins a meaningful number of marks", totalMarks > 40)
    }

    /**
     * The grain is part of the shared spec, so it is locked from this side too.
     * A single divergent cell — a sign-extended shift, a non-wrapping multiply —
     * moves the checksum and fails here.
     */
    @Test
    fun textureTileMatchesTheSharedSpec() {
        val texture = fixture.getJSONObject("texture")
        assertEquals(texture.getInt("tile_size"), PAPER_TEXTURE_TILE_SIZE)
        assertEquals(texture.getString("color"), PAPER_TEXTURE_COLOR)
        assertEquals(texture.getInt("max_alpha"), PAPER_TEXTURE_MAX_ALPHA)

        val tile = paperTextureTile()
        assertEquals(PAPER_TEXTURE_TILE_SIZE * PAPER_TEXTURE_TILE_SIZE, tile.size)
        assertEquals(texture.getInt("covered_cells"), tile.count { it > 0 })

        // Same fold as the Rust generator, in the same order.
        var checksum = 0uL
        tile.forEachIndexed { index, alpha ->
            checksum = checksum * 31uL + (index.toULong() xor alpha.toULong())
        }
        assertEquals(texture.getString("checksum"), checksum.toString())

        val samples = texture.getJSONArray("samples")
        for (index in 0 until samples.length()) {
            val sample = samples.getJSONObject(index)
            val x = sample.getInt("x")
            val y = sample.getInt("y")
            assertEquals(
                "grain at $x,$y",
                sample.getInt("alpha"),
                paperTextureAlpha(x, y),
            )
        }

        assertTrue(paperHasTexture(Paper.RuledNarrow))
        assertTrue(!paperHasTexture(Paper.None))
    }

    /**
     * Squared papers are ruled papers plus verticals: the horizontals must not
     * move, and the margin must land on a vertical in both grids.
     */
    @Test
    fun gridsAlignWithRulesAndTheMarginLandsOnAVertical() {
        assertEquals(Paper.RuledNarrow.ruleSpacing, Paper.SquaredSmall.ruleSpacing)
        assertEquals(Paper.RuledWide.ruleSpacing, Paper.SquaredLarge.ruleSpacing)
        assertEquals(Paper.RuledMarginNarrow.ruleSpacing, Paper.SquaredSmall.ruleSpacing)
        assertEquals(Paper.RuledMarginWide.ruleSpacing, Paper.SquaredLarge.ruleSpacing)

        listOf(Paper.SquaredSmall, Paper.SquaredLarge).forEach { paper ->
            val pitch = paper.columnSpacing!!
            assertEquals("margin misses the ${paper.wireValue} grid", 0.0, MARGIN_X % pitch, 0.0)
        }

        val viewport = PaperViewport(-200.0, -200.0, 400.0, 400.0, 1.0)
        val horizontals = { paper: Paper ->
            paperMarks(paper, viewport).filter { it.kind == PaperMarkKind.Rule }.map { it.position }
        }
        assertEquals(horizontals(Paper.RuledNarrow), horizontals(Paper.SquaredSmall))
        assertEquals(horizontals(Paper.RuledWide), horizontals(Paper.SquaredLarge))
    }

    @Test
    fun wireValuesRoundTripAndRejectUnknowns() {
        for (paper in Paper.ALL) {
            assertEquals(paper, Paper.fromWire(paper.wireValue))
        }
        assertEquals(null, Paper.fromWire("ruled-margin-huge"))
        assertEquals(null, Paper.fromWire(""))
        assertEquals(null, Paper.fromWire(null))
    }

    /** Every swatch size must clear the cull, or the palette icon renders blank. */
    @Test
    fun previewViewportNeverCulls() {
        for (paper in Paper.ALL) {
            for ((width, height) in listOf(24.0 to 18.0, 88.0 to 66.0, 256.0 to 192.0)) {
                val viewport = previewViewport(paper, width, height)
                assertTrue(viewport.scale > 0.0 && viewport.scale.isFinite())
                if (paper == Paper.None) {
                    assertTrue(paperMarks(paper, viewport).isEmpty())
                    continue
                }
                listOfNotNull(paper.ruleSpacing, paper.columnSpacing).forEach { pitch ->
                    assertTrue(
                        "${paper.wireValue} culled in a ${width}x$height swatch",
                        paperFamilyVisible(pitch, viewport.scale),
                    )
                }
                assertTrue(
                    "${paper.wireValue} swatch is blank at ${width}x$height",
                    paperMarks(paper, viewport).isNotEmpty(),
                )
            }
        }
    }

    /** A pathological viewport terminates at the cap rather than enumerating forever. */
    @Test
    fun pathologicalViewportHitsTheAxisCap() {
        val viewport = PaperViewport(-1e12, -1e12, 1e12, 1e12, 1.0)
        assertEquals(2 * MAX_PAPER_MARKS_PER_AXIS, paperMarks(Paper.SquaredSmall, viewport).size)

        assertTrue(
            paperMarks(
                Paper.SquaredSmall,
                PaperViewport(Double.NEGATIVE_INFINITY, 0.0, Double.POSITIVE_INFINITY, 10.0, 1.0),
            ).isEmpty(),
        )
        assertTrue(paperMarks(Paper.RuledNarrow, PaperViewport(100.0, 100.0, -100.0, -100.0, 1.0)).isEmpty())
        assertTrue(paperMarks(Paper.RuledNarrow, PaperViewport(-100.0, -100.0, 100.0, 100.0, 0.0)).isEmpty())
    }
}

/** The fixture spells kinds in kebab-case, matching `PaperMarkKind`'s serde form. */
internal val PaperMarkKind.wireKind: String
    get() =
        when (this) {
            PaperMarkKind.Rule -> "rule"
            PaperMarkKind.Column -> "column"
            PaperMarkKind.Margin -> "margin"
        }
