package link.desync.vnote.api.codec

import link.desync.vnote.model.PageEvent
import link.desync.vnote.model.SolidRoundParameters
import link.desync.vnote.model.Stroke
import link.desync.vnote.model.StrokePoint
import link.desync.vnote.model.StrokeStyle
import org.json.JSONArray
import org.json.JSONObject

// JSON codecs for canonical ink, shared by the page channel's server messages
// (`stroke-batch`) and its client messages (`commit-batch`).

// A sequenced stroke batch: the body of a `stroke-batch` message, and the shape
// of each batch in a page replay.
internal fun decodeStrokeBatch(json: JSONObject): PageEvent.StrokeBatch =
    PageEvent.StrokeBatch(
        seq = json.getLong("seq"),
        clientBatchId = json.getString("client_batch_id"),
        strokes = decodeStrokes(json.getJSONArray("strokes")),
    )

internal fun decodeStrokes(array: JSONArray): List<Stroke> =
    buildList {
        for (index in 0 until array.length()) {
            add(decodeStroke(array.getJSONObject(index)))
        }
    }

internal fun decodeStroke(json: JSONObject): Stroke {
    val pointsArray = json.getJSONArray("points")
    val points =
        buildList {
            for (index in 0 until pointsArray.length()) {
                val point = pointsArray.getJSONObject(index)
                add(
                    StrokePoint(
                        x = point.getDouble("x"),
                        y = point.getDouble("y"),
                        t = point.getLong("t"),
                        pressure =
                            if (point.has("pressure") && !point.isNull("pressure")) {
                                point.getDouble("pressure")
                            } else {
                                null
                            },
                    ),
                )
            }
        }
    return Stroke(
        points = points,
        id = json.getString("id"),
        style = decodeStrokeStyle(json.getJSONObject("style")),
    )
}

internal fun encodeStroke(stroke: Stroke): JSONObject {
    val pointsArray = JSONArray()
    for (point in stroke.points) {
        val pointJson =
            JSONObject()
                .put("x", point.x)
                .put("y", point.y)
                .put("t", point.t)
        // Emit pressure only when present, so v1 strokes stay byte-identical
        // on the wire (absent, never `null`).
        point.pressure?.let { pointJson.put("pressure", it) }
        pointsArray.put(pointJson)
    }
    return JSONObject()
        .put("id", stroke.id)
        .put("style", encodeStrokeStyle(stroke.style))
        .put("points", pointsArray)
}

private fun decodeStrokeStyle(json: JSONObject): StrokeStyle {
    val parameters = json.getJSONObject("parameters")
    return StrokeStyle(
        toolKind = json.getString("tool_kind"),
        styleVersion = json.getInt("style_version"),
        parameters =
            SolidRoundParameters(
                color = parameters.getString("color"),
                width = parameters.getDouble("width"),
                capStyle = parameters.getString("cap_style"),
                joinStyle = parameters.getString("join_style"),
            ),
    )
}

private fun encodeStrokeStyle(style: StrokeStyle): JSONObject =
    JSONObject()
        .put("tool_kind", style.toolKind)
        .put("style_version", style.styleVersion)
        .put(
            "parameters",
            JSONObject()
                .put("color", style.parameters.color)
                .put("width", style.parameters.width)
                .put("cap_style", style.parameters.capStyle)
                .put("join_style", style.parameters.joinStyle),
        )
