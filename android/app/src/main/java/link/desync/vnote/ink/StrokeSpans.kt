package link.desync.vnote.ink

import link.desync.vnote.model.Stroke
import link.desync.vnote.telemetry.OpenSpan
import link.desync.vnote.telemetry.SpanContext
import link.desync.vnote.telemetry.Telemetry
import link.desync.vnote.telemetry.TelemetryRuntime
import link.desync.vnote.telemetry.millisToNanos

// The ink pipeline for one committed stroke, as spans (#406):
//
//     ink.stroke     pen down ─────────────────────────────── server echo
//       ink.capture  pen down ── pen up
//       ink.commit                  commit-batch sent ─────── server echo
//
// `ink.stroke` is pen-to-confirmed-ink as the user experiences it, and the
// number #318 turns into a latency figure. It is opened at commit time and
// backdated to pen down: a stroke's points carry their offset from pen down, so
// the capture needs no timing hooks in the input path.
//
// The server's handling of the batch is not a child of `ink.commit`: the page
// channel carries no per-message trace context (#354's Q7, decided on #406 as
// "the same as the SPA"). The connection the batch travels on *is* in the page's
// trace — the upgrade carried `traceparent` — and the server links each
// message's own trace to that connection. `vnote.client_batch_id` joins the two.
internal class StrokeSpans private constructor(
    private val stroke: OpenSpan,
    private val commit: OpenSpan,
) {
    fun confirmed(seq: Long) {
        commit.attr("vnote.seq", seq).end()
        stroke.end()
    }

    fun failed(reason: String) {
        commit.fail(reason)
        stroke.fail(reason)
    }

    companion object {
        fun open(
            screen: SpanContext,
            clientBatchId: String,
            submitted: Stroke,
            telemetry: TelemetryRuntime = Telemetry.runtime,
        ): StrokeSpans {
            val now = telemetry.now()
            val penDown = now - millisToNanos(submitted.points.lastOrNull()?.t ?: 0L)
            val stroke =
                telemetry
                    .span("ink.stroke", screen, startUnixNanos = penDown)
                    .attr("vnote.client_batch_id", clientBatchId)
            telemetry
                .span("ink.capture", stroke.context, startUnixNanos = penDown)
                .attr("vnote.points", submitted.points.size)
                .end(now)
            val commit =
                telemetry
                    .span("ink.commit", stroke.context, startUnixNanos = now)
                    .attr("vnote.client_batch_id", clientBatchId)
            return StrokeSpans(stroke, commit)
        }
    }
}
