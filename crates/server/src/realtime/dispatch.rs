//! What each inbound page-channel message does: lease handling, commits,
//! paper changes, and replay. Transport-agnostic — replies go to any
//! `Sink<Message>`, persistence goes through [`PageStore`] — so every message
//! type is unit-tested below against an in-memory store.

use std::collections::HashSet;
use std::time::Instant;

use axum::extract::ws::Message;
use futures_util::{Sink, SinkExt};
use protocol::{
    LibraryEvent, PageClientMessage, PageServerMessage, Paper, Stroke, StrokeBatch,
    ThumbnailMetadata, TombstoneBatch,
};

use super::hub::LeaseOutcome;
use super::store::PageStore;
use crate::AppState;

/// The connection a page-channel message arrived on: the shared state and the
/// store it acts on, the page, and the sending session.
pub(super) struct PageContext<'a, S> {
    pub(super) state: &'a AppState,
    pub(super) store: &'a S,
    pub(super) page_id: &'a str,
    pub(super) session_id: &'a str,
}

/// Routes one parsed page-channel message to its handler. Returns false when
/// the socket should close (a send failed).
pub(super) async fn dispatch_page_client_message<S, Tx>(
    ctx: &PageContext<'_, S>,
    sender: &mut Tx,
    message: PageClientMessage,
    received: Instant,
) -> bool
where
    S: PageStore,
    Tx: Sink<Message> + Unpin,
{
    match message {
        PageClientMessage::Subscribe { from_seq } => {
            replay_page(ctx.store, ctx.page_id, sender, from_seq, received).await
        }
        PageClientMessage::AcquireLease => acquire_lease(ctx, sender).await,
        PageClientMessage::RenewLease => renew_lease(ctx, sender).await,
        PageClientMessage::ReleaseLease => {
            release_lease(ctx);
            true
        }
        PageClientMessage::CommitBatch {
            client_batch_id,
            strokes,
        } => commit_batch(ctx, sender, client_batch_id, strokes).await,
        PageClientMessage::CommitTombstones {
            client_mutation_id,
            stroke_ids,
        } => commit_tombstones(ctx, sender, client_mutation_id, stroke_ids).await,
        PageClientMessage::SetPaper {
            client_mutation_id,
            paper,
        } => set_paper(ctx, sender, client_mutation_id, paper).await,
    }
}

async fn acquire_lease<S, Tx>(ctx: &PageContext<'_, S>, sender: &mut Tx) -> bool
where
    Tx: Sink<Message> + Unpin,
{
    match ctx
        .state
        .realtime
        .acquire_lease(ctx.page_id, ctx.session_id)
    {
        LeaseOutcome::Granted => {
            ctx.state.realtime.publish_page(
                ctx.page_id,
                PageServerMessage::LeaseChanged {
                    holder: Some(ctx.session_id.to_string()),
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

async fn renew_lease<S, Tx>(ctx: &PageContext<'_, S>, sender: &mut Tx) -> bool
where
    Tx: Sink<Message> + Unpin,
{
    match ctx
        .state
        .realtime
        .acquire_lease(ctx.page_id, ctx.session_id)
    {
        LeaseOutcome::Granted => send_page(sender, PageServerMessage::LeaseGranted).await,
        LeaseOutcome::Denied { holder } => {
            crate::observability::metrics().record_realtime_event("page", "lease_denied");
            send_page(sender, PageServerMessage::LeaseDenied { holder }).await
        }
    }
}

fn release_lease<S>(ctx: &PageContext<'_, S>) {
    if ctx
        .state
        .realtime
        .release_lease(ctx.page_id, ctx.session_id)
    {
        ctx.state.realtime.publish_page(
            ctx.page_id,
            PageServerMessage::LeaseChanged { holder: None },
        );
    }
}

async fn commit_batch<S, Tx>(
    ctx: &PageContext<'_, S>,
    sender: &mut Tx,
    client_batch_id: String,
    strokes: Vec<Stroke>,
) -> bool
where
    S: PageStore,
    Tx: Sink<Message> + Unpin,
{
    // Single active editor: only the lease holder may ink. Acquiring
    // also renews the holder's lease on each commit.
    if let LeaseOutcome::Denied { holder } = ctx
        .state
        .realtime
        .acquire_lease(ctx.page_id, ctx.session_id)
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
    let persisted = match ctx
        .store
        .persist_batch(ctx.page_id, &client_batch_id, &strokes)
        .await
    {
        Ok(persisted) => persisted,
        Err(error) => {
            tracing::error!(error = %error, "stroke commit failed");
            crate::observability::metrics().record_page_mutation("commit_batch", "error");
            return send_page(
                sender,
                PageServerMessage::Error {
                    code: "commit_failed".to_string(),
                    message: "could not persist strokes".to_string(),
                    client_mutation_id: None,
                },
            )
            .await;
        }
    };
    crate::observability::metrics().record_page_mutation("commit_batch", "success");
    // Delete-wins: broadcast only strokes that survived tombstone
    // filtering. An add fully suppressed by tombstones changes no
    // visible state, so it is acknowledged without a fan-out.
    if !persisted.visible_strokes.is_empty() {
        ctx.state.realtime.publish_page(
            ctx.page_id,
            PageServerMessage::StrokeBatch(StrokeBatch {
                seq: persisted.seq,
                client_batch_id,
                strokes: persisted.visible_strokes,
            }),
        );
    }
    if let Some(updated_at) = persisted.updated_at {
        publish_page_updated(ctx, &persisted.owner_id, updated_at);
    }
    if persisted.thumbnail_job_created {
        crate::observability::metrics().thumbnail_generation_queued();
        queue_thumbnail(ctx, persisted.owner_id, persisted.revision);
    }
    true
}

async fn commit_tombstones<S, Tx>(
    ctx: &PageContext<'_, S>,
    sender: &mut Tx,
    client_mutation_id: String,
    stroke_ids: Vec<String>,
) -> bool
where
    S: PageStore,
    Tx: Sink<Message> + Unpin,
{
    // Unlike commit_batch and set_paper, this path records no `lease_denied`
    // event and no `thumbnail_generation_queued`. Carried over as found by
    // #337, which only restructures; the fix is tracked in #339.
    if let LeaseOutcome::Denied { holder } = ctx
        .state
        .realtime
        .acquire_lease(ctx.page_id, ctx.session_id)
    {
        return send_page(sender, PageServerMessage::LeaseDenied { holder }).await;
    }
    let persisted = match ctx
        .store
        .persist_tombstones(ctx.page_id, &client_mutation_id, &stroke_ids)
        .await
    {
        Ok(persisted) => persisted,
        Err(error) => {
            tracing::error!(error = %error, "tombstone commit failed");
            crate::observability::metrics().record_page_mutation("commit_tombstones", "error");
            return send_page(
                sender,
                PageServerMessage::Error {
                    code: "tombstone_failed".to_string(),
                    message: "could not persist deleted strokes".to_string(),
                    client_mutation_id: Some(client_mutation_id),
                },
            )
            .await;
        }
    };
    ctx.state.realtime.publish_page(
        ctx.page_id,
        PageServerMessage::TombstoneBatch(TombstoneBatch {
            revision: persisted.revision,
            client_mutation_id,
            stroke_ids: persisted.stroke_ids,
        }),
    );
    if let Some(updated_at) = persisted.updated_at {
        publish_page_updated(ctx, &persisted.owner_id, updated_at);
    }
    if persisted.thumbnail_job_created {
        queue_thumbnail(ctx, persisted.owner_id, persisted.revision);
    }
    crate::observability::metrics().record_page_mutation("commit_tombstones", "success");
    true
}

async fn set_paper<S, Tx>(
    ctx: &PageContext<'_, S>,
    sender: &mut Tx,
    client_mutation_id: String,
    paper: Paper,
) -> bool
where
    S: PageStore,
    Tx: Sink<Message> + Unpin,
{
    // Paper is a visible page mutation that bumps the revision, mints a
    // thumbnail and re-sorts the library — exactly the class the
    // single-editor invariant governs. Acquiring also renews the holder's
    // lease, identically to CommitBatch/CommitTombstones.
    if let LeaseOutcome::Denied { holder } = ctx
        .state
        .realtime
        .acquire_lease(ctx.page_id, ctx.session_id)
    {
        crate::observability::metrics().record_realtime_event("page", "lease_denied");
        return send_page(sender, PageServerMessage::LeaseDenied { holder }).await;
    }
    let persisted = match ctx.store.persist_paper(ctx.page_id, paper).await {
        Ok(persisted) => persisted,
        Err(error) => {
            tracing::error!(error = %error, "paper change failed");
            crate::observability::metrics().record_page_mutation("set_paper", "error");
            return send_page(
                sender,
                PageServerMessage::Error {
                    code: "paper_failed".to_string(),
                    message: "could not persist the page paper".to_string(),
                    client_mutation_id: Some(client_mutation_id),
                },
            )
            .await;
        }
    };
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
    ctx.state.realtime.publish_page(
        ctx.page_id,
        PageServerMessage::PaperChanged {
            paper,
            revision: persisted.revision,
        },
    );
    if let Some(updated_at) = persisted.updated_at {
        publish_page_updated(ctx, &persisted.owner_id, updated_at);
    }
    if persisted.thumbnail_job_created {
        crate::observability::metrics().thumbnail_generation_queued();
        queue_thumbnail(ctx, persisted.owner_id, persisted.revision);
    }
    crate::observability::metrics().record_page_mutation("set_paper", "success");
    true
}

/// Tells the owner's library the page was just edited, so it re-sorts.
fn publish_page_updated<S>(ctx: &PageContext<'_, S>, owner_id: &str, updated_at: String) {
    ctx.state.realtime.publish_library_event(
        owner_id,
        LibraryEvent::PageUpdated {
            page_id: ctx.page_id.to_string(),
            updated_at,
        },
    );
}

/// Announces the thumbnail job the store just created, then starts rendering it.
fn queue_thumbnail<S>(ctx: &PageContext<'_, S>, owner_id: String, revision: u64) {
    ctx.state.realtime.publish_library_event(
        &owner_id,
        LibraryEvent::PageThumbnailUpdated {
            page_id: ctx.page_id.to_string(),
            thumbnail: ThumbnailMetadata::Generating {
                source_seq: revision,
            },
        },
    );
    crate::thumbnails::enqueue(
        ctx.state.clone(),
        ctx.page_id.to_string(),
        owner_id,
        revision,
    );
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
async fn replay_page<S, Tx>(
    store: &S,
    page_id: &str,
    sender: &mut Tx,
    from_seq: u64,
    received: Instant,
) -> bool
where
    S: PageStore,
    Tx: Sink<Message> + Unpin,
{
    let (batches, tombstones) = match store.load_page_replay(page_id, from_seq).await {
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
    let last_seq = store.max_seq(page_id).await.unwrap_or(from_seq);
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
    //! Every page-channel message type against an in-memory [`PageStore`].
    //!
    //! Lease-*granted* replies are deliberately not produced here:
    //! `send_page_frame_records_the_bytes_it_sent` asserts an exact
    //! process-wide count for that message type.

    use std::sync::Mutex;

    use tokio::sync::broadcast;

    use super::*;
    use crate::realtime::hub::Fanout;
    use crate::realtime::store::{PersistedBatch, PersistedPaper, PersistedTombstones};

    const PAGE: &str = "page_1";
    const SESSION: &str = "session_a";
    const OWNER: &str = "owner_1";
    const EDITED_AT: &str = "2026-09-15T00:00:00+00:00";

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

    /// Canned results for each mutation, taken on first use — a test that does
    /// not set one up proves dispatch never reached the store for it — plus a
    /// log of every call dispatch made.
    #[derive(Default)]
    struct FakeStore {
        batch: Mutex<Option<Result<PersistedBatch, sqlx::Error>>>,
        tombstones: Mutex<Option<Result<PersistedTombstones, sqlx::Error>>>,
        paper: Mutex<Option<Result<PersistedPaper, sqlx::Error>>>,
        replay: (Vec<StrokeBatch>, Vec<TombstoneBatch>),
        head_seq: u64,
        calls: Mutex<Vec<String>>,
    }

    impl FakeStore {
        fn record(&self, call: String) {
            self.calls.lock().expect("calls lock").push(call);
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().expect("calls lock").clone()
        }

        fn take<T>(
            slot: &Mutex<Option<Result<T, sqlx::Error>>>,
            call: &str,
        ) -> Result<T, sqlx::Error> {
            slot.lock()
                .expect("slot lock")
                .take()
                .unwrap_or_else(|| panic!("dispatch made an unexpected {call} call"))
        }
    }

    fn joined<'a>(ids: impl Iterator<Item = &'a str>) -> String {
        ids.collect::<Vec<_>>().join(",")
    }

    impl PageStore for FakeStore {
        async fn page_belongs_to_owner(
            &self,
            _page_id: &str,
            _owner_id: &str,
        ) -> Result<bool, sqlx::Error> {
            unreachable!("ownership is checked at upgrade, never by dispatch")
        }

        async fn current_paper(&self, _page_id: &str) -> Result<Paper, sqlx::Error> {
            unreachable!("paper is read for the welcome, never by dispatch")
        }

        async fn max_seq(&self, page_id: &str) -> Result<u64, sqlx::Error> {
            self.record(format!("max_seq {page_id}"));
            Ok(self.head_seq)
        }

        async fn load_page_replay(
            &self,
            page_id: &str,
            from_seq: u64,
        ) -> Result<(Vec<StrokeBatch>, Vec<TombstoneBatch>), sqlx::Error> {
            self.record(format!("load_page_replay {page_id} {from_seq}"));
            Ok(self.replay.clone())
        }

        async fn persist_batch(
            &self,
            page_id: &str,
            client_batch_id: &str,
            strokes: &[Stroke],
        ) -> Result<PersistedBatch, sqlx::Error> {
            let ids = joined(strokes.iter().map(|stroke| stroke.id.as_str()));
            self.record(format!("persist_batch {page_id} {client_batch_id} {ids}"));
            Self::take(&self.batch, "persist_batch")
        }

        async fn persist_tombstones(
            &self,
            page_id: &str,
            client_mutation_id: &str,
            stroke_ids: &[String],
        ) -> Result<PersistedTombstones, sqlx::Error> {
            let ids = joined(stroke_ids.iter().map(String::as_str));
            self.record(format!(
                "persist_tombstones {page_id} {client_mutation_id} {ids}"
            ));
            Self::take(&self.tombstones, "persist_tombstones")
        }

        async fn persist_paper(
            &self,
            page_id: &str,
            paper: Paper,
        ) -> Result<PersistedPaper, sqlx::Error> {
            self.record(format!("persist_paper {page_id} {}", paper.wire_value()));
            Self::take(&self.paper, "persist_paper")
        }
    }

    /// Dispatches `message` from [`SESSION`] on [`PAGE`]. Returns whether the
    /// socket stays open and every frame sent straight back to the sender.
    async fn handle(
        state: &AppState,
        store: &FakeStore,
        message: PageClientMessage,
    ) -> (bool, Vec<PageServerMessage>) {
        let ctx = PageContext {
            state,
            store,
            page_id: PAGE,
            session_id: SESSION,
        };
        let mut sink: Vec<Message> = Vec::new();
        let keep_open =
            dispatch_page_client_message(&ctx, &mut sink, message, Instant::now()).await;
        let replies = text_frames(&sink)
            .into_iter()
            .map(|frame| serde_json::from_str(frame).expect("a page server message"))
            .collect();
        (keep_open, replies)
    }

    /// Everything broadcast on a channel so far, without waiting.
    fn drain<T: Clone>(receiver: &mut broadcast::Receiver<Fanout<T>>) -> Vec<T> {
        std::iter::from_fn(|| receiver.try_recv().ok().map(|fanout| fanout.message)).collect()
    }

    fn commit(ids: &[&str]) -> PageClientMessage {
        PageClientMessage::CommitBatch {
            client_batch_id: "batch_1".to_string(),
            strokes: ids.iter().copied().map(fixture_stroke).collect(),
        }
    }

    fn persisted_batch(visible_strokes: Vec<Stroke>) -> PersistedBatch {
        PersistedBatch {
            seq: 5,
            revision: 9,
            owner_id: OWNER.to_string(),
            visible_strokes,
            thumbnail_job_created: false,
            updated_at: Some(EDITED_AT.to_string()),
        }
    }

    fn page_updated() -> LibraryEvent {
        LibraryEvent::PageUpdated {
            page_id: PAGE.to_string(),
            updated_at: EDITED_AT.to_string(),
        }
    }

    fn set_paper_message() -> PageClientMessage {
        PageClientMessage::SetPaper {
            client_mutation_id: "paper_1".to_string(),
            paper: Paper::RuledWide,
        }
    }

    fn persisted_paper(changed: bool) -> PersistedPaper {
        PersistedPaper {
            changed,
            revision: 3,
            owner_id: OWNER.to_string(),
            thumbnail_job_created: false,
            updated_at: changed.then(|| EDITED_AT.to_string()),
        }
    }

    fn hold_lease(state: &AppState, session_id: &str) {
        assert!(matches!(
            state.realtime.acquire_lease(PAGE, session_id),
            LeaseOutcome::Granted
        ));
    }

    // ---- commit-batch -------------------------------------------------------

    #[tokio::test]
    async fn commit_batch_persists_then_fans_out_only_the_visible_strokes() {
        let state = AppState::for_tests();
        let mut page = state.realtime.subscribe_page(PAGE);
        let mut library = state.realtime.subscribe_library(OWNER);
        let store = FakeStore {
            batch: Mutex::new(Some(Ok(persisted_batch(vec![fixture_stroke("kept")])))),
            ..FakeStore::default()
        };

        let (keep_open, replies) = handle(&state, &store, commit(&["kept", "erased"])).await;

        assert!(keep_open);
        assert!(
            replies.is_empty(),
            "a commit is acknowledged by its fan-out, not a reply"
        );
        assert_eq!(store.calls(), ["persist_batch page_1 batch_1 kept,erased"]);
        assert_eq!(
            drain(&mut page),
            [PageServerMessage::StrokeBatch(StrokeBatch {
                seq: 5,
                client_batch_id: "batch_1".to_string(),
                strokes: vec![fixture_stroke("kept")],
            })]
        );
        assert_eq!(drain(&mut library), [page_updated()]);
    }

    #[tokio::test]
    async fn commit_batch_without_the_lease_is_denied_before_reaching_the_store() {
        let state = AppState::for_tests();
        hold_lease(&state, "session_b");
        let store = FakeStore::default();

        let (keep_open, replies) = handle(&state, &store, commit(&["kept"])).await;

        assert!(keep_open);
        assert_eq!(
            replies,
            [PageServerMessage::LeaseDenied {
                holder: "session_b".to_string()
            }]
        );
        assert!(store.calls().is_empty());
    }

    #[tokio::test]
    async fn commit_batch_with_an_invalid_style_is_rejected_before_reaching_the_store() {
        let state = AppState::for_tests();
        let mut stroke = fixture_stroke("bad");
        stroke.style.parameters.width = 0.3;
        let store = FakeStore::default();
        let message = PageClientMessage::CommitBatch {
            client_batch_id: "batch_1".to_string(),
            strokes: vec![stroke],
        };

        let (keep_open, replies) = handle(&state, &store, message).await;

        assert!(keep_open);
        assert_eq!(
            replies,
            [PageServerMessage::Error {
                code: "invalid_stroke_style".to_string(),
                message: "strokes require a supported immutable solid_round style with \
                          in-range pressure"
                    .to_string(),
                client_mutation_id: None,
            }]
        );
        assert!(store.calls().is_empty());
    }

    #[tokio::test]
    async fn commit_batch_store_failure_replies_commit_failed_and_broadcasts_nothing() {
        let state = AppState::for_tests();
        let mut page = state.realtime.subscribe_page(PAGE);
        let store = FakeStore {
            batch: Mutex::new(Some(Err(sqlx::Error::RowNotFound))),
            ..FakeStore::default()
        };

        let (keep_open, replies) = handle(&state, &store, commit(&["kept"])).await;

        assert!(keep_open);
        assert_eq!(
            replies,
            [PageServerMessage::Error {
                code: "commit_failed".to_string(),
                message: "could not persist strokes".to_string(),
                client_mutation_id: None,
            }]
        );
        assert!(drain(&mut page).is_empty());
    }

    #[tokio::test]
    async fn a_batch_fully_suppressed_by_tombstones_is_acknowledged_without_a_fan_out() {
        let state = AppState::for_tests();
        let mut page = state.realtime.subscribe_page(PAGE);
        let mut library = state.realtime.subscribe_library(OWNER);
        let store = FakeStore {
            batch: Mutex::new(Some(Ok(PersistedBatch {
                updated_at: None,
                ..persisted_batch(Vec::new())
            }))),
            ..FakeStore::default()
        };

        let (keep_open, replies) = handle(&state, &store, commit(&["erased"])).await;

        assert!(keep_open);
        assert!(replies.is_empty());
        assert!(drain(&mut page).is_empty());
        assert!(drain(&mut library).is_empty());
    }

    #[tokio::test]
    async fn a_commit_that_creates_a_thumbnail_job_announces_it_as_generating() {
        let state = AppState::for_tests();
        let mut library = state.realtime.subscribe_library(OWNER);
        let store = FakeStore {
            batch: Mutex::new(Some(Ok(PersistedBatch {
                thumbnail_job_created: true,
                updated_at: None,
                ..persisted_batch(vec![fixture_stroke("kept")])
            }))),
            ..FakeStore::default()
        };

        let (keep_open, _) = handle(&state, &store, commit(&["kept"])).await;

        assert!(keep_open);
        // Read before this task yields: the render job dispatch spawned has not
        // run, so its eventual `failed` update (the test pool never connects)
        // cannot interleave.
        assert_eq!(
            drain(&mut library),
            [LibraryEvent::PageThumbnailUpdated {
                page_id: PAGE.to_string(),
                thumbnail: ThumbnailMetadata::Generating { source_seq: 9 },
            }]
        );
    }

    // ---- commit-tombstones --------------------------------------------------

    #[tokio::test]
    async fn commit_tombstones_broadcasts_the_batch_the_store_recorded() {
        let state = AppState::for_tests();
        let mut page = state.realtime.subscribe_page(PAGE);
        let mut library = state.realtime.subscribe_library(OWNER);
        let store = FakeStore {
            tombstones: Mutex::new(Some(Ok(PersistedTombstones {
                revision: 4,
                owner_id: OWNER.to_string(),
                stroke_ids: vec!["s1".to_string()],
                thumbnail_job_created: false,
                updated_at: Some(EDITED_AT.to_string()),
            }))),
            ..FakeStore::default()
        };
        let message = PageClientMessage::CommitTombstones {
            client_mutation_id: "erase_1".to_string(),
            stroke_ids: vec!["s1".to_string(), "s1".to_string()],
        };

        let (keep_open, replies) = handle(&state, &store, message).await;

        assert!(keep_open);
        assert!(replies.is_empty());
        // Dedupe is the store's job: dispatch passes the request through as sent.
        assert_eq!(store.calls(), ["persist_tombstones page_1 erase_1 s1,s1"]);
        assert_eq!(
            drain(&mut page),
            [PageServerMessage::TombstoneBatch(TombstoneBatch {
                revision: 4,
                client_mutation_id: "erase_1".to_string(),
                stroke_ids: vec!["s1".to_string()],
            })]
        );
        assert_eq!(drain(&mut library), [page_updated()]);
    }

    #[tokio::test]
    async fn commit_tombstones_failure_echoes_the_client_mutation_id() {
        let state = AppState::for_tests();
        let store = FakeStore {
            tombstones: Mutex::new(Some(Err(sqlx::Error::RowNotFound))),
            ..FakeStore::default()
        };
        let message = PageClientMessage::CommitTombstones {
            client_mutation_id: "erase_1".to_string(),
            stroke_ids: vec!["s1".to_string()],
        };

        let (keep_open, replies) = handle(&state, &store, message).await;

        assert!(keep_open);
        assert_eq!(
            replies,
            [PageServerMessage::Error {
                code: "tombstone_failed".to_string(),
                message: "could not persist deleted strokes".to_string(),
                client_mutation_id: Some("erase_1".to_string()),
            }]
        );
    }

    // ---- set-paper ----------------------------------------------------------

    #[tokio::test]
    async fn set_paper_to_the_paper_already_in_force_is_acked_directly() {
        let state = AppState::for_tests();
        let mut page = state.realtime.subscribe_page(PAGE);
        let store = FakeStore {
            paper: Mutex::new(Some(Ok(persisted_paper(false)))),
            ..FakeStore::default()
        };

        let (keep_open, replies) = handle(&state, &store, set_paper_message()).await;

        assert!(keep_open);
        assert_eq!(
            replies,
            [PageServerMessage::PaperChanged {
                paper: Paper::RuledWide,
                revision: 3,
            }]
        );
        assert!(drain(&mut page).is_empty(), "a no-op is not fanned out");
        assert_eq!(store.calls(), ["persist_paper page_1 ruled-wide"]);
    }

    #[tokio::test]
    async fn a_real_paper_change_is_broadcast_rather_than_replied() {
        let state = AppState::for_tests();
        let mut page = state.realtime.subscribe_page(PAGE);
        let mut library = state.realtime.subscribe_library(OWNER);
        let store = FakeStore {
            paper: Mutex::new(Some(Ok(persisted_paper(true)))),
            ..FakeStore::default()
        };

        let (keep_open, replies) = handle(&state, &store, set_paper_message()).await;

        assert!(keep_open);
        assert!(
            replies.is_empty(),
            "the sender hears the change through the broadcast"
        );
        assert_eq!(
            drain(&mut page),
            [PageServerMessage::PaperChanged {
                paper: Paper::RuledWide,
                revision: 3,
            }]
        );
        assert_eq!(drain(&mut library), [page_updated()]);
    }

    // ---- subscribe ----------------------------------------------------------

    #[tokio::test]
    async fn subscribe_replays_surviving_ink_then_tombstones_then_synced() {
        let state = AppState::for_tests();
        let tombstones = TombstoneBatch {
            revision: 2,
            client_mutation_id: "erase_1".to_string(),
            stroke_ids: vec!["erased".to_string()],
        };
        let store = FakeStore {
            replay: (
                vec![StrokeBatch {
                    seq: 1,
                    client_batch_id: "batch_1".to_string(),
                    strokes: vec![fixture_stroke("kept"), fixture_stroke("erased")],
                }],
                vec![tombstones.clone()],
            ),
            head_seq: 1,
            ..FakeStore::default()
        };

        let (keep_open, replies) =
            handle(&state, &store, PageClientMessage::Subscribe { from_seq: 0 }).await;

        assert!(keep_open);
        assert_eq!(
            replies,
            [
                PageServerMessage::StrokeBatch(StrokeBatch {
                    seq: 1,
                    client_batch_id: "batch_1".to_string(),
                    strokes: vec![fixture_stroke("kept")],
                }),
                PageServerMessage::TombstoneBatch(tombstones),
                PageServerMessage::Synced { last_seq: 1 },
            ]
        );
        assert_eq!(
            store.calls(),
            ["load_page_replay page_1 0", "max_seq page_1"]
        );
    }

    // ---- leases -------------------------------------------------------------

    #[tokio::test]
    async fn acquiring_or_renewing_a_lease_another_session_holds_is_denied() {
        let state = AppState::for_tests();
        hold_lease(&state, "session_b");
        let mut page = state.realtime.subscribe_page(PAGE);

        for message in [
            PageClientMessage::AcquireLease,
            PageClientMessage::RenewLease,
        ] {
            let (keep_open, replies) = handle(&state, &FakeStore::default(), message).await;
            assert!(keep_open);
            assert_eq!(
                replies,
                [PageServerMessage::LeaseDenied {
                    holder: "session_b".to_string()
                }]
            );
        }
        assert!(drain(&mut page).is_empty());
    }

    #[tokio::test]
    async fn releasing_a_held_lease_tells_every_session_the_page_is_free() {
        let state = AppState::for_tests();
        hold_lease(&state, SESSION);
        let mut page = state.realtime.subscribe_page(PAGE);

        let (keep_open, replies) = handle(
            &state,
            &FakeStore::default(),
            PageClientMessage::ReleaseLease,
        )
        .await;

        assert!(keep_open);
        assert!(replies.is_empty());
        assert_eq!(
            drain(&mut page),
            [PageServerMessage::LeaseChanged { holder: None }]
        );
        assert_eq!(state.realtime.current_lease_holder(PAGE), None);
    }

    #[tokio::test]
    async fn only_the_holder_can_release_a_lease() {
        let state = AppState::for_tests();
        hold_lease(&state, "session_b");
        let mut page = state.realtime.subscribe_page(PAGE);

        let (keep_open, replies) = handle(
            &state,
            &FakeStore::default(),
            PageClientMessage::ReleaseLease,
        )
        .await;

        assert!(keep_open);
        assert!(replies.is_empty());
        assert!(drain(&mut page).is_empty());
        assert_eq!(
            state.realtime.current_lease_holder(PAGE).as_deref(),
            Some("session_b")
        );
    }

    // ---- frame chokepoint ---------------------------------------------------

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
