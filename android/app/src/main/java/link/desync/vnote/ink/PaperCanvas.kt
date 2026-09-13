package link.desync.vnote.ink

import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.ImageShader
import androidx.compose.ui.graphics.ShaderBrush
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.TileMode
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.graphics.drawscope.DrawScope
import androidx.compose.ui.graphics.drawscope.Stroke as DrawStroke

/**
 * Draw the page's paper behind the ink.
 *
 * Called from *inside* the canvas's existing `withTransform` block, so paper
 * shares the ink transform and its size and position relative to the ink match
 * the SPA and thumbnails exactly, through pan and zoom alike.
 */
internal fun DrawScope.drawPaperMarks(
    paper: Paper,
    viewport: ViewportTransform,
    size: Size,
) {
    if (paper == Paper.None || viewport.scale <= 0f || !viewport.scale.isFinite()) {
        return
    }
    val topLeft = viewport.toWorld(Offset.Zero)
    val bottomRight = viewport.toWorld(Offset(size.width, size.height))
    val paperViewport =
        PaperViewport(
            minX = topLeft.x.toDouble(),
            minY = topLeft.y.toDouble(),
            maxX = bottomRight.x.toDouble(),
            maxY = bottomRight.y.toDouble(),
            scale = viewport.scale.toDouble(),
        )

    // Only two colours exist, so parse them once per draw rather than
    // re-parsing a hex string for every mark on every frame.
    val ruleColor = parseColor(RULE_COLOR)
    val marginColor = parseColor(MARGIN_COLOR)

    visitPaperMarks(paper, paperViewport) { mark ->
        // The DrawScope is already scaled by the transform, so the device-space
        // width floor has to be divided back out to survive it unchanged.
        val strokeWidth =
            (paperMarkDeviceWidth(mark.worldWidth, paperViewport.scale) / paperViewport.scale)
                .toFloat()
        val position = mark.position.toFloat()
        // Butt caps: a round cap would bulge each line's ends past the viewport.
        val start: Offset
        val end: Offset
        if (mark.kind.isHorizontal) {
            start = Offset(paperViewport.minX.toFloat(), position)
            end = Offset(paperViewport.maxX.toFloat(), position)
        } else {
            start = Offset(position, paperViewport.minY.toFloat())
            end = Offset(position, paperViewport.maxY.toFloat())
        }
        drawLine(
            color = if (mark.kind == PaperMarkKind.Margin) marginColor else ruleColor,
            start = start,
            end = end,
            strokeWidth = strokeWidth,
            cap = StrokeCap.Butt,
        )
    }
}

/**
 * Draw a paper swatch filling [size], using [previewViewport] so the marks
 * always clear the density cull — a swatch of 48-unit rules at its natural scale
 * would be culled and the icon would render blank.
 */
internal fun DrawScope.drawPaperPreview(
    paper: Paper,
    size: Size,
) {
    if (paper == Paper.None) {
        return
    }
    val viewport = previewViewport(paper, size.width.toDouble(), size.height.toDouble())
    val scale = viewport.scale
    val ruleColor = parseColor(RULE_COLOR)
    val marginColor = parseColor(MARGIN_COLOR)
    visitPaperMarks(paper, viewport) { mark ->
        val strokeWidth = paperMarkDeviceWidth(mark.worldWidth, scale).toFloat()
        // Preview draws in device space directly, so map world → swatch pixels.
        val position = ((mark.position - if (mark.kind.isHorizontal) viewport.minY else viewport.minX) * scale).toFloat()
        val start: Offset
        val end: Offset
        if (mark.kind.isHorizontal) {
            start = Offset(0f, position)
            end = Offset(size.width, position)
        } else {
            start = Offset(position, 0f)
            end = Offset(position, size.height)
        }
        drawLine(
            color = if (mark.kind == PaperMarkKind.Margin) marginColor else ruleColor,
            start = start,
            end = end,
            strokeWidth = strokeWidth,
            cap = StrokeCap.Butt,
        )
    }
    // A hairline frame keeps the swatch legible against the menu surface; it is
    // chrome, not paper, so it is drawn outside the mark loop.
    drawRect(
        color = ruleColor,
        style = DrawStroke(width = 1f),
    )
}

/**
 * The repeating grain, built once for the process.
 *
 * The tile is a pure function of the shared spec, so it never varies between
 * pages, users or frames — building it per frame would allocate a 64×64 bitmap
 * on every pan and zoom step.
 */
private val paperTextureBrush: ShaderBrush by lazy {
    val size = PAPER_TEXTURE_TILE_SIZE
    val alphas = paperTextureTile()
    val grain = parseColor(PAPER_TEXTURE_COLOR)
    val red = (grain.red * 255f).toInt()
    val green = (grain.green * 255f).toInt()
    val blue = (grain.blue * 255f).toInt()
    val pixels =
        IntArray(alphas.size) { index ->
            (alphas[index] shl 24) or (red shl 16) or (green shl 8) or blue
        }
    val bitmap =
        android.graphics.Bitmap.createBitmap(
            pixels,
            size,
            size,
            android.graphics.Bitmap.Config.ARGB_8888,
        )
    ShaderBrush(
        ImageShader(
            bitmap.asImageBitmap(),
            TileMode.Repeated,
            TileMode.Repeated,
        ),
    )
}

/**
 * Lay the faint paper grain over the whole canvas, under both the rules and the
 * ink.
 *
 * Called **outside** the viewport transform, so the grain tiles in device space
 * and keeps a constant perceptual size at every zoom. Tiling it in world space
 * would turn the speckle into visible blocks when zoomed in and dissolve it
 * entirely when zoomed out.
 */
internal fun DrawScope.drawPaperTexture(paper: Paper) {
    if (!paperHasTexture(paper)) {
        return
    }
    drawRect(brush = paperTextureBrush, size = size)
}
