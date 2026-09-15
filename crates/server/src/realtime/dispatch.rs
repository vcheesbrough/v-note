//! What each inbound page-channel message does: lease handling, commits,
//! paper changes, and replay. Transport-agnostic — replies go to any
//! `Sink<Message>`, persistence goes through `store`.

use std::collections::HashSet;
use std::time::Instant;

use axum::extract::ws::{Message, WebSocket};
use futures_util::stream::SplitSink;
use futures_util::{Sink, SinkExt};
use protocol::{LibraryEvent, PageClientMessage, PageServerMessage, StrokeBatch, TombstoneBatch};
use sqlx::PgPool;

use super::hub::LeaseOutcome;
use super::store::{load_page_replay, max_seq, persist_batch, persist_paper, persist_tombstones};
use crate::AppState;

pub(super) async fn dispatch_page_client_message(
    state: &AppState,
    pool: &PgPool,
    page_id: &str,
    session_id: &str,
    sender: &mut SplitSink<WebSocket, Message>,
    message: PageClientMessage,
    received: Instant,
) -> bool {
    match message {
        PageClientMessage::Subscribe { from_seq } => {
            replay_page(pool, page_id, sender, from_seq, received).await
        }
        PageClientMessage::AcquireLease => {
            match state.realtime.acquire_lease(page_id, session_id) {
                LeaseOutcome::Granted => {
                    state.realtime.publish_page(
                        page_id,
                        PageServerMessage::LeaseChanged {
                            holder: Some(session_id.to_string()),
                        },
                    );
                    send_page(sender, PageServerMessage::LeaseGranted).await
                }
                LeaseOutcome::Denied { holder } => {
                    crate::observability::metrics().record_realtime_event("page", "lease_denied");
                    send_page(sender, PageServerMessage::LeaseDenied { holder }).await
                }
            }
        }
        PageClientMessage::RenewLease => match state.realtime.acquire_lease(page_id, session_id) {
            LeaseOutcome::Granted => send_page(sender, PageServerMessage::LeaseGranted).await,
            LeaseOutcome::Denied { holder } => {
                crate::observability::metrics().record_realtime_event("page", "lease_denied");
                send_page(sender, PageServerMessage::LeaseDenied { holder }).await
            }
        },
        PageClientMessage::ReleaseLease => {
            if state.realtime.release_lease(page_id, session_id) {
                state
                    .realtime
                    .publish_page(page_id, PageServerMessage::LeaseChanged { holder: None });
            }
            true
        }
        PageClientMessage::CommitBatch {
            client_batch_id,
            strokes,
        } => {
            // Single active editor: only the lease holder may ink. Acquiring
            // also renews the holder's lease on each commit.
            if let LeaseOutcome::Denied { holder } =
                state.realtime.acquire_lease(page_id, session_id)
            {
                crate::observability::metrics().record_realtime_event("page", "lease_denied");
                return send_page(sender, PageServerMessage::LeaseDenied { holder }).await;
            }
            if strokes
                .iter()
                .any(|stroke| stroke.id.is_empty() || stroke.validate().is_err())
            {
                return send_page(
                    sender,
                    PageServerMessage::Error {
                        code: "invalid_stroke_style".to_string(),
                        message: "strokes require a supported immutable solid_round style \
                                  with in-range pressure"
                            .to_string(),
                        client_mutation_id: None,
                    },
                )
                .await;
            }
            match persist_batch(pool, page_id, &client_batch_id, &strokes).await {
                Ok(persisted) => {
                    let seq = persisted.seq;
                    crate::observability::metrics().record_page_mutation("commit_batch", "success");
                    // Delete-wins: broadcast only strokes that survived tombstone
                    // filtering. An add fully suppressed by tombstones changes no
                    // visible state, so it is acknowledged without a fan-out.
                    if !persisted.visible_strokes.is_empty() {
                        state.realtime.publish_page(
                            page_id,
                            PageServerMessage::StrokeBatch(StrokeBatch {
                                seq,
                                client_batch_id,
                                strokes: persisted.visible_strokes,
                            }),
                        );
                    }
                    if let Some(updated_at) = persisted.updated_at {
                        state.realtime.publish_library_event(
                            &persisted.owner_id,
                            LibraryEvent::PageUpdated {
                                page_id: page_id.to_string(),
                                updated_at,
                            },
                        );
                    }
                    if persisted.thumbnail_job_created {
                        let page_id = page_id.to_string();
                        crate::observability::metrics().thumbnail_generation_queued();
                        state.realtime.publish_library_event(
                            &persisted.owner_id,
                            LibraryEvent::PageThumbnailUpdated {
                                page_id: page_id.clone(),
                                thumbnail: protocol::ThumbnailMetadata::Generating {
                                    source_seq: persisted.revision,
                                },
                            },
                        );
                        crate::thumbnails::enqueue(
                            state.clone(),
                            page_id,
                            persisted.owner_id,
                            persisted.revision,
                        );
                    }
                    true
                }
                Err(error) => {
                    tracing::error!(error = %error, "stroke commit failed");
                    crate::observability::metrics().record_page_mutation("commit_batch", "error");
                    send_page(
                        sender,
                        PageServerMessage::Error {
                            code: "commit_failed".to_string(),
                            message: "could not persist strokes".to_string(),
                            client_mutation_id: None,
                        },
                    )
                    .await
                }
            }
        }
        PageClientMessage::CommitTombstones {
            client_mutation_id,
            stroke_ids,
        } => {
            if let LeaseOutcome::Denied { holder } =
                state.realtime.acquire_lease(page_id, session_id)
            {
                return send_page(sender, PageServerMessage::LeaseDenied { holder }).await;
            }
            match persist_tombstones(pool, page_id, &client_mutation_id, &stroke_ids).await {
                Ok(persisted) => {
                    let event = TombstoneBatch {
                        revision: persisted.revision,
                        client_mutation_id,
                        stroke_ids: persisted.stroke_ids,
                    };
                    state
                        .realtime
                        .publish_page(page_id, PageServerMessage::TombstoneBatch(event));
                    if let Some(updated_at) = persisted.updated_at {
                        state.realtime.publish_library_event(
                            &persisted.owner_id,
                            LibraryEvent::PageUpdated {
                                page_id: page_id.to_string(),
                                updated_at,
                            },
                        );
                    }
                    if persisted.thumbnail_job_created {
                        state.realtime.publish_library_event(
                            &persisted.owner_id,
                            LibraryEvent::PageThumbnailUpdated {
                                page_id: page_id.to_string(),
                                thumbnail: protocol::ThumbnailMetadata::Generating {
                                    source_seq: persisted.revision,
                                },
                            },
                        );
                        crate::thumbnails::enqueue(
                            state.clone(),
                            page_id.to_string(),
                            persisted.owner_id,
                            persisted.revision,
                        );
                    }
                    crate::observability::metrics()
                        .record_page_mutation("commit_tombstones", "success");
                    true
                }
                Err(error) => {
                    tracing::error!(error = %error, "tombstone commit failed");
                    crate::observability::metrics()
                        .record_page_mutation("commit_tombstones", "error");
                    send_page(
                        sender,
                        PageServerMessage::Error {
                            code: "tombstone_failed".to_string(),
                            message: "could not persist deleted strokes".to_string(),
                            client_mutation_id: Some(client_mutation_id),
                        },
                    )
                    .await
                }
            }
        }
        PageClientMessage::SetPaper {
            client_mutation_id,
            paper,
        } => {
            // Paper is a visible page mutation that bumps the revision, mints a
            // thumbnail and re-sorts the library — exactly the class the
            // single-editor invariant governs. Acquiring also renews the holder's
            // lease, identically to CommitBatch/CommitTombstones.
            if let LeaseOutcome::Denied { holder } =
                state.realtime.acquire_lease(page_id, session_id)
            {
                crate::observability::metrics().record_realtime_event("page", "lease_denied");
                return send_page(sender, PageServerMessage::LeaseDenied { holder }).await;
            }
            match persist_paper(pool, page_id, paper).await {
                Ok(persisted) => {
                    if !persisted.changed {
                        // Value-idempotent: nothing bumped, nothing fanned out.
                        // Ack directly so a racing client still converges.
                        crate::observability::metrics().record_page_mutation("set_paper", "noop");
                        return send_page(
                            sender,
                            PageServerMessage::PaperChanged {
                                paper,
                                revision: persisted.revision,
                            },
                        )
                        .await;
                    }
                    // The broadcast reaches the sender too, as with StrokeBatch.
                    state.realtime.publish_page(
                        page_id,
                        PageServerMessage::PaperChanged {
                            paper,
                            revision: persisted.revision,
                        },
                    );
                    if let Some(updated_at) = persisted.updated_at {
                        state.realtime.publish_library_event(
                            &persisted.owner_id,
                            LibraryEvent::PageUpdated {
                                page_id: page_id.to_string(),
                                updated_at,
                            },
                        );
                    }
                    if persisted.thumbnail_job_created {
                        crate::observability::metrics().thumbnail_generation_queued();
                        state.realtime.publish_library_event(
                            &persisted.owner_id,
                            LibraryEvent::PageThumbnailUpdated {
                                page_id: page_id.to_string(),
                                thumbnail: protocol::ThumbnailMetadata::Generating {
                                    source_seq: persisted.revision,
                                },
                            },
                        );
                        crate::thumbnails::enqueue(
                            state.clone(),
                            page_id.to_string(),
                            persisted.owner_id,
                            persisted.revision,
                        );
                    }
                    crate::observability::metrics().record_page_mutation("set_paper", "success");
                    true
                }
                Err(error) => {
                    tracing::error!(error = %error, "paper change failed");
                    crate::observability::metrics().record_page_mutation("set_paper", "error");
                    send_page(
                        sender,
                        PageServerMessage::Error {
                            code: "paper_failed".to_string(),
                            message: "could not persist the page paper".to_string(),
                            client_mutation_id: Some(client_mutation_id),
                        },
                    )
                    .await
                }
            }
        }
    }
}

/// What one replay put on the wire.
#[derive(Debug, Default, PartialEq, Eq)]
struct ReplayCost {
    frames: u64,
    bytes: u64,
}

impl ReplayCost {
    fn add_frame(&mut self, bytes: usize) {
        self.frames += 1;
        self.bytes += bytes as u64;
    }
}

/// Replays a page to one subscriber: surviving stroke batches, every tombstone
/// batch, then `synced`. Its frames, bytes and duration are recorded once
/// `synced` is sent; a replay cut short by a failed read or send records none.
#[tracing::instrument(skip_all, fields(from_seq = from_seq, frames, bytes))]
async fn replay_page(
    pool: &PgPool,
    page_id: &str,
    sender: &mut SplitSink<WebSocket, Message>,
    from_seq: u64,
    received: Instant,
) -> bool {
    let (batches, tombstones) = match load_page_replay(pool, page_id, from_seq).await {
        Ok(replay) => replay,
        Err(error) => {
            tracing::error!(error = %error, "stroke replay failed");
            crate::observability::metrics().record_realtime_event("page", "replay_error");
            return send_page(
                sender,
                PageServerMessage::Error {
                    code: "replay_failed".to_string(),
                    message: "could not load page ink".to_string(),
                    client_mutation_id: None,
                },
            )
            .await;
        }
    };
    let mut cost = ReplayCost::default();
    if !send_replay_frames(sender, batches, tombstones, &mut cost).await {
        return false;
    }
    let last_seq = max_seq(pool, page_id).await.unwrap_or(from_seq);
    let Some(synced_bytes) = send_page_frame(sender, PageServerMessage::Synced { last_seq }).await
    else {
        return false;
    };
    cost.add_frame(synced_bytes);

    let span = tracing::Span::current();
    span.record("frames", cost.frames);
    span.record("bytes", cost.bytes);
    crate::observability::metrics().observe_realtime_replay(
        cost.frames,
        cost.bytes,
        received.elapsed().as_secs_f64(),
    );
    true
}

/// Sends a replay's stroke-batch and tombstone-batch frames, adding each to
/// `cost`. Returns false when a send fails.
async fn send_replay_frames<S>(
    sender: &mut S,
    batches: Vec<StrokeBatch>,
    tombstones: Vec<TombstoneBatch>,
    cost: &mut ReplayCost,
) -> bool
where
    S: Sink<Message> + Unpin,
{
    let deleted_ids: HashSet<&str> = tombstones
        .iter()
        .flat_map(|batch| batch.stroke_ids.iter().map(String::as_str))
        .collect();
    for mut batch in batches {
        batch
            .strokes
            .retain(|stroke| !deleted_ids.contains(stroke.id.as_str()));
        let Some(bytes) = send_page_frame(sender, PageServerMessage::StrokeBatch(batch)).await
        else {
            return false;
        };
        cost.add_frame(bytes);
    }
    // Replaying tombstones is required even when `from_seq` skips their
    // source stroke batches: a reconnecting client may still cache them.
    for batch in tombstones {
        let Some(bytes) = send_page_frame(sender, PageServerMessage::TombstoneBatch(batch)).await
        else {
            return false;
        };
        cost.add_frame(bytes);
    }
    true
}

pub(super) async fn send_page<S>(sender: &mut S, message: PageServerMessage) -> bool
where
    S: Sink<Message> + Unpin,
{
    send_page_frame(sender, message).await.is_some()
}

/// Serializes and sends one page-channel frame, returning its size in bytes, or
/// `None` when the socket should close. Every server→client page frame passes
/// through here, so this is where frame size is recorded — after the send, so
/// the histogram counts only frames that reached the socket.
pub(super) async fn send_page_frame<S>(sender: &mut S, message: PageServerMessage) -> Option<usize>
where
    S: Sink<Message> + Unpin,
{
    let message_type = message.message_type();
    let payload = serde_json::to_string(&message).ok()?;
    let bytes = payload.len();
    sender.send(Message::Text(payload.into())).await.ok()?;
    crate::observability::metrics().observe_realtime_message_bytes("page", message_type, bytes);
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use protocol::Stroke;

    use super::*;

    fn fixture_stroke(id: &str) -> Stroke {
        let mut stroke: Stroke =
            serde_json::from_str(include_str!("../../../../contracts/fixtures/stroke.json"))
                .expect("stroke fixture should parse");
        stroke.id = id.to_string();
        stroke
    }

    fn text_frames(sink: &[Message]) -> Vec<&str> {
        sink.iter()
            .map(|message| match message {
                Message::Text(text) => text.as_str(),
                other => panic!("expected a text frame, got {other:?}"),
            })
            .collect()
    }

    // `send_page_frame` is generic over the sink precisely so the chokepoint can
    // be exercised without a socket: a `Vec<Message>` is a sink that never fails.
    #[tokio::test]
    async fn send_page_frame_records_the_bytes_it_sent() {
        let metrics = crate::observability::metrics();
        let before = metrics.realtime_message_count("page", "lease-granted");
        let mut sink: Vec<Message> = Vec::new();

        let bytes = send_page_frame(&mut sink, PageServerMessage::LeaseGranted)
            .await
            .expect("a Vec sink never fails");

        let frames = text_frames(&sink);
        assert_eq!(frames, [r#"{"type":"lease-granted"}"#]);
        assert_eq!(bytes, frames[0].len());
        // No other test in this binary sends `lease-granted`, so the delta on the
        // process-wide histogram is exact.
        assert_eq!(
            metrics.realtime_message_count("page", "lease-granted"),
            before + 1
        );
    }

    #[tokio::test]
    async fn replay_frames_apply_delete_wins_and_count_every_frame() {
        let batches = vec![
            StrokeBatch {
                seq: 1,
                client_batch_id: "batch_1".to_string(),
                strokes: vec![fixture_stroke("kept"), fixture_stroke("erased")],
            },
            StrokeBatch {
                seq: 2,
                client_batch_id: "batch_2".to_string(),
                strokes: vec![fixture_stroke("later")],
            },
        ];
        let tombstones = vec![TombstoneBatch {
            revision: 3,
            client_mutation_id: "erase_1".to_string(),
            stroke_ids: vec!["erased".to_string()],
        }];
        let mut sink: Vec<Message> = Vec::new();
        let mut cost = ReplayCost::default();

        assert!(send_replay_frames(&mut sink, batches, tombstones, &mut cost).await);

        let frames = text_frames(&sink);
        let types: Vec<String> = frames
            .iter()
            .map(|frame| {
                serde_json::from_str::<serde_json::Value>(frame).expect("frame should be JSON")
                    ["type"]
                    .as_str()
                    .expect("frame should carry a type")
                    .to_string()
            })
            .collect();
        // One frame per stored batch plus one per tombstone batch — the shape
        // #323 collapses into a single frame.
        assert_eq!(types, ["stroke-batch", "stroke-batch", "tombstone-batch"]);
        assert!(frames[0].contains(r#""id":"kept""#));
        assert!(
            !frames[0].contains(r#""id":"erased""#),
            "delete-wins filtering must still drop tombstoned strokes"
        );
        assert_eq!(
            cost,
            ReplayCost {
                frames: 3,
                bytes: frames.iter().map(|frame| frame.len() as u64).sum(),
            }
        );
    }
}
