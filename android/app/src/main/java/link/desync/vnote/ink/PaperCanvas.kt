package link.desync.vnote.ink

import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.StrokeCap
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
            color = parseColor(mark.kind.color),
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
            color = parseColor(mark.kind.color),
            start = start,
            end = end,
            strokeWidth = strokeWidth,
            cap = StrokeCap.Butt,
        )
    }
    // A hairline frame keeps the swatch legible against the menu surface; it is
    // chrome, not paper, so it is drawn outside the mark loop.
    drawRect(
        color = parseColor(RULE_COLOR),
        style = DrawStroke(width = 1f),
    )
}
