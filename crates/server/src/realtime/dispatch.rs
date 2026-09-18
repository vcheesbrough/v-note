//! What each inbound page-channel message does: lease handling, commits,
//! paper changes, and replay. Transport-agnostic — replies go to any
//! `Sink<Message>`, persistence goes through [`PageStore`] — so every message
//! type is unit-tested below against an in-memory store.

use std::collections::HashSet;
use std::time::Instant;

use axum::extract::ws::Message;
use futures_util::{Sink, SinkExt};
use protocol::{
    LibraryEvent, PageClientMessage, PageReplay, PageServerMessage, Paper, Stroke, StrokeBatch,
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
            replay_page(
                ctx.store,
                ctx.page_id,
                sender,
                from_seq,
                received,
                ctx.state.coalesce_replay,
            )
            .await
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
    if let LeaseOutcome::Denied { holder } = ctx
        .state
        .realtime
        .acquire_lease(ctx.page_id, ctx.session_id)
    {
        crate::observability::metrics().record_realtime_event("page", "lease_denied");
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
/// The queue-depth gauge is counted by `thumbnails::enqueue`, so every mutation
/// that mints a job moves it identically.
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

/// Replays a page to one subscriber.
///
/// With `coalesce` (the `realtime.coalesce-replay` default) it is a **single**
/// `page-replay` frame: surviving stroke batches, every tombstone batch, and the
/// head `seq` the subscriber is now caught up to. With the flag off it is the
/// pre-#323 shape — a `stroke-batch` per stored batch, a `tombstone-batch` per
/// tombstone batch, then `synced` — which every client still understands, so the
/// flag is a runtime rollback rather than a rebuild.
///
/// Its frames, bytes and duration are recorded once the last frame is sent; a
/// replay cut short by a failed read or send records none.
#[tracing::instrument(skip_all, fields(from_seq = from_seq, coalesced = coalesce, frames, bytes))]
async fn replay_page<S, Tx>(
    store: &S,
    page_id: &str,
    sender: &mut Tx,
    from_seq: u64,
    received: Instant,
    coalesce: bool,
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
    let last_seq = store.max_seq(page_id).await.unwrap_or(from_seq);
    let mut cost = ReplayCost::default();
    if coalesce {
        let replay = build_page_replay(page_id, last_seq, batches, tombstones);
        let Some(bytes) = send_page_frame(sender, PageServerMessage::PageReplay(replay)).await
        else {
            return false;
        };
        cost.add_frame(bytes);
    } else {
        if !send_replay_frames(sender, batches, tombstones, &mut cost).await {
            return false;
        }
        let Some(bytes) = send_page_frame(sender, PageServerMessage::Synced { last_seq }).await
        else {
            return false;
        };
        cost.add_frame(bytes);
    }

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

/// Assembles one replay payload: stroke batches with delete-wins applied, and
/// every tombstone batch. Tombstones ride along even when `from_seq` skips the
/// batches they deleted from, because a reconnecting client may still cache
/// those strokes.
fn build_page_replay(
    page_id: &str,
    last_seq: u64,
    mut batches: Vec<StrokeBatch>,
    tombstones: Vec<TombstoneBatch>,
) -> PageReplay {
    let deleted_ids: HashSet<&str> = tombstones
        .iter()
        .flat_map(|batch| batch.stroke_ids.iter().map(String::as_str))
        .collect();
    for batch in &mut batches {
        batch
            .strokes
            .retain(|stroke| !deleted_ids.contains(stroke.id.as_str()));
    }
    PageReplay {
        page_id: page_id.to_string(),
        last_seq,
        batches,
        tombstones,
    }
}

/// The pre-#323 replay shape, kept behind `realtime.coalesce-replay = false`:
/// one `stroke-batch` frame per stored batch, then one per tombstone batch,
/// adding each to `cost`. Returns false when a send fails.
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
    //! `send_page_frame_records_the_bytes_it_sent` asserts an exact
    //! process-wide frame count for `lease-changed`, which dispatch only ever
    //! broadcasts and never sends as a reply, so no other test here moves it.
    //! Tests asserting exact deltas on the `lease_denied` counter or the
    //! thumbnail queue-depth gauge hold [`METRICS_LOCK`], as does every other
    //! test that moves either.

    use std::sync::Mutex;

    use tokio::sync::broadcast;

    use super::*;
    use crate::realtime::hub::Fanout;
    use crate::realtime::store::{PersistedBatch, PersistedPaper, PersistedTombstones};

    const PAGE: &str = "page_1";
    const SESSION: &str = "session_a";
    const OWNER: &str = "owner_1";
    const EDITED_AT: &str = "2026-09-15T00:00:00+00:00";

    /// Serializes the tests that move process-wide metrics they assert on.
    static METRICS_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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

    fn erase_message() -> PageClientMessage {
        PageClientMessage::CommitTombstones {
            client_mutation_id: "erase_1".to_string(),
            stroke_ids: vec!["s1".to_string()],
        }
    }

    fn persisted_tombstones() -> PersistedTombstones {
        PersistedTombstones {
            revision: 9,
            owner_id: OWNER.to_string(),
            stroke_ids: vec!["s1".to_string()],
            thumbnail_job_created: false,
            updated_at: None,
        }
    }

    /// One of each lease-guarded mutation, labelled for assertion messages.
    fn every_mutation() -> [(&'static str, PageClientMessage); 3] {
        [
            ("commit-batch", commit(&["kept"])),
            ("commit-tombstones", erase_message()),
            ("set-paper", set_paper_message()),
        ]
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
    async fn every_mutation_without_the_lease_is_denied_and_counted_before_reaching_the_store() {
        let _metrics = METRICS_LOCK.lock().await;
        let metrics = crate::observability::metrics();
        let state = AppState::for_tests();
        hold_lease(&state, "session_b");

        for (name, message) in every_mutation() {
            let before = metrics.realtime_event_count("page", "lease_denied");
            let store = FakeStore::default();

            let (keep_open, replies) = handle(&state, &store, message).await;

            assert!(keep_open, "{name}");
            assert_eq!(
                replies,
                [PageServerMessage::LeaseDenied {
                    holder: "session_b".to_string()
                }],
                "{name}"
            );
            assert!(store.calls().is_empty(), "{name}");
            assert_eq!(
                metrics.realtime_event_count("page", "lease_denied"),
                before + 1,
                "{name} should count its lease denial"
            );
        }
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

    // Leaves the process-wide `v_note_thumbnail_queue_depth` gauge three higher:
    // the render jobs it spawns are cancelled with the test runtime, so their
    // decrements never run. Only this test moves that gauge in this binary.
    #[tokio::test]
    async fn every_mutation_that_creates_a_thumbnail_job_queues_and_announces_it() {
        let _metrics = METRICS_LOCK.lock().await;
        let metrics = crate::observability::metrics();

        for (name, message) in every_mutation() {
            let state = AppState::for_tests();
            let mut library = state.realtime.subscribe_library(OWNER);
            let store = FakeStore {
                batch: Mutex::new(Some(Ok(PersistedBatch {
                    thumbnail_job_created: true,
                    updated_at: None,
                    ..persisted_batch(vec![fixture_stroke("kept")])
                }))),
                tombstones: Mutex::new(Some(Ok(PersistedTombstones {
                    thumbnail_job_created: true,
                    ..persisted_tombstones()
                }))),
                paper: Mutex::new(Some(Ok(PersistedPaper {
                    revision: 9,
                    thumbnail_job_created: true,
                    updated_at: None,
                    ..persisted_paper(true)
                }))),
                ..FakeStore::default()
            };
            let before = metrics.thumbnail_queue_depth();

            let (keep_open, _) = handle(&state, &store, message).await;

            assert!(keep_open, "{name}");
            // Read before this task yields: the render job dispatch spawned has
            // not run, so neither its `finished` decrement nor its eventual
            // `failed` update (the test pool never connects) can interleave.
            assert_eq!(
                metrics.thumbnail_queue_depth(),
                before + 1,
                "{name} should count its thumbnail job as queued"
            );
            assert_eq!(
                drain(&mut library),
                [LibraryEvent::PageThumbnailUpdated {
                    page_id: PAGE.to_string(),
                    thumbnail: ThumbnailMetadata::Generating { source_seq: 9 },
                }],
                "{name}"
            );
        }
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

    /// The whole replay is **one** `page-replay` message — surviving ink,
    /// every tombstone batch, and the head `seq` that used to arrive as a
    /// separate `synced` (#323).
    #[tokio::test]
    async fn subscribe_replays_surviving_ink_and_tombstones_in_one_message() {
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
            [PageServerMessage::PageReplay(PageReplay {
                page_id: PAGE.to_string(),
                last_seq: 1,
                batches: vec![StrokeBatch {
                    seq: 1,
                    client_batch_id: "batch_1".to_string(),
                    strokes: vec![fixture_stroke("kept")],
                }],
                tombstones: vec![tombstones],
            })]
        );
        assert_eq!(
            store.calls(),
            ["load_page_replay page_1 0", "max_seq page_1"]
        );
    }

    /// `realtime.coalesce-replay = false` restores the pre-#323 wire shape, so
    /// the change can be rolled back at runtime rather than by rebuilding. The
    /// ink and the tombstones delivered are identical; only the framing differs.
    #[tokio::test]
    async fn the_flag_off_restores_the_per_message_replay_shape() {
        let mut state = AppState::for_tests();
        state.coalesce_replay = false;
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
            ],
            "delete-wins still applied, and `synced` still closes the replay"
        );
    }

    /// Both shapes are counted by the same #324 instrumentation, so the
    /// `frames` histogram is what tells an operator which shape is in force.
    #[tokio::test]
    async fn both_replay_shapes_are_counted_by_the_same_instrumentation() {
        let batches: Vec<StrokeBatch> = (1..=3)
            .map(|seq| StrokeBatch {
                seq,
                client_batch_id: format!("batch_{seq}"),
                strokes: vec![fixture_stroke(&format!("stroke_{seq}"))],
            })
            .collect();
        let store = || FakeStore {
            replay: (batches.clone(), Vec::new()),
            head_seq: 3,
            ..FakeStore::default()
        };

        let mut coalesced: Vec<Message> = Vec::new();
        assert!(replay_page(&store(), PAGE, &mut coalesced, 0, Instant::now(), true).await);
        let mut per_message: Vec<Message> = Vec::new();
        assert!(replay_page(&store(), PAGE, &mut per_message, 0, Instant::now(), false).await);

        assert_eq!(text_frames(&coalesced).len(), 1);
        // 3 x stroke-batch + synced — the shape #323 replaced.
        assert_eq!(text_frames(&per_message).len(), 4);
    }

    /// Gap-fill: `from_seq` is passed through to the store untouched, and the
    /// head `seq` the client is told about is the store's, not `from_seq`.
    #[tokio::test]
    async fn subscribe_from_a_seq_asks_the_store_for_only_later_batches() {
        let state = AppState::for_tests();
        let store = FakeStore {
            replay: (
                vec![StrokeBatch {
                    seq: 8,
                    client_batch_id: "batch_8".to_string(),
                    strokes: vec![fixture_stroke("late")],
                }],
                Vec::new(),
            ),
            head_seq: 8,
            ..FakeStore::default()
        };

        let (keep_open, replies) =
            handle(&state, &store, PageClientMessage::Subscribe { from_seq: 7 }).await;

        assert!(keep_open);
        match &replies[..] {
            [PageServerMessage::PageReplay(replay)] => {
                assert_eq!(replay.last_seq, 8);
                assert_eq!(
                    replay
                        .batches
                        .iter()
                        .map(|batch| batch.seq)
                        .collect::<Vec<_>>(),
                    vec![8]
                );
            }
            other => panic!("expected one page-replay, got {other:?}"),
        }
        assert_eq!(
            store.calls(),
            ["load_page_replay page_1 7", "max_seq page_1"]
        );
    }

    /// A reconnecting client may still cache strokes a tombstone deleted, so
    /// tombstones replay even when `from_seq` skips their source batches.
    #[tokio::test]
    async fn subscribe_replays_tombstones_even_when_from_seq_skips_their_batches() {
        let state = AppState::for_tests();
        let tombstones = TombstoneBatch {
            revision: 4,
            client_mutation_id: "erase_1".to_string(),
            stroke_ids: vec!["erased".to_string()],
        };
        let store = FakeStore {
            replay: (Vec::new(), vec![tombstones.clone()]),
            head_seq: 9,
            ..FakeStore::default()
        };

        let (keep_open, replies) =
            handle(&state, &store, PageClientMessage::Subscribe { from_seq: 9 }).await;

        assert!(keep_open);
        assert_eq!(
            replies,
            [PageServerMessage::PageReplay(PageReplay {
                page_id: PAGE.to_string(),
                last_seq: 9,
                batches: Vec::new(),
                tombstones: vec![tombstones],
            })]
        );
    }

    // ---- leases -------------------------------------------------------------

    #[tokio::test]
    async fn acquiring_a_free_lease_replies_granted_and_announces_the_new_holder() {
        let state = AppState::for_tests();
        let mut page = state.realtime.subscribe_page(PAGE);

        let (keep_open, replies) = handle(
            &state,
            &FakeStore::default(),
            PageClientMessage::AcquireLease,
        )
        .await;

        assert!(keep_open);
        assert_eq!(replies, [PageServerMessage::LeaseGranted]);
        assert_eq!(
            drain(&mut page),
            [PageServerMessage::LeaseChanged {
                holder: Some(SESSION.to_string())
            }]
        );
        assert_eq!(
            state.realtime.current_lease_holder(PAGE).as_deref(),
            Some(SESSION)
        );
    }

    #[tokio::test]
    async fn renewing_a_held_lease_replies_granted_without_a_broadcast() {
        let state = AppState::for_tests();
        hold_lease(&state, SESSION);
        let mut page = state.realtime.subscribe_page(PAGE);

        let (keep_open, replies) =
            handle(&state, &FakeStore::default(), PageClientMessage::RenewLease).await;

        assert!(keep_open);
        assert_eq!(replies, [PageServerMessage::LeaseGranted]);
        assert!(drain(&mut page).is_empty(), "a renewal changes no holder");
    }

    #[tokio::test]
    async fn acquiring_or_renewing_a_lease_another_session_holds_is_denied() {
        let _metrics = METRICS_LOCK.lock().await;
        let metrics = crate::observability::metrics();
        let before = metrics.realtime_event_count("page", "lease_denied");
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
        assert_eq!(
            metrics.realtime_event_count("page", "lease_denied"),
            before + 2
        );
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
        let before = metrics.realtime_message_count("page", "lease-changed");
        let mut sink: Vec<Message> = Vec::new();

        let bytes = send_page_frame(&mut sink, PageServerMessage::LeaseChanged { holder: None })
            .await
            .expect("a Vec sink never fails");

        let frames = text_frames(&sink);
        assert_eq!(frames, [r#"{"type":"lease-changed"}"#]);
        assert_eq!(bytes, frames[0].len());
        // No other test in this binary sends `lease-changed` as a frame, so the delta on the
        // process-wide histogram is exact.
        assert_eq!(
            metrics.realtime_message_count("page", "lease-changed"),
            before + 1
        );
    }

    /// N stored batches cost **one** frame, whatever N is, and the #324
    /// `frames`/`bytes` instrumentation counts that one frame (#323).
    #[tokio::test]
    async fn a_dense_replay_is_one_frame_whatever_the_batch_count() {
        let batches: Vec<StrokeBatch> = (1..=50)
            .map(|seq| StrokeBatch {
                seq,
                client_batch_id: format!("batch_{seq}"),
                strokes: vec![fixture_stroke(&format!("stroke_{seq}"))],
            })
            .collect();
        let store = FakeStore {
            replay: (batches, Vec::new()),
            head_seq: 50,
            ..FakeStore::default()
        };
        let mut sink: Vec<Message> = Vec::new();

        assert!(
            replay_page(&store, PAGE, &mut sink, 0, Instant::now(), true).await,
            "a successful replay keeps the socket open"
        );

        let frames = text_frames(&sink);
        assert_eq!(frames.len(), 1, "50 stored batches must cost one frame");
        let payload: serde_json::Value =
            serde_json::from_str(frames[0]).expect("frame should be JSON");
        assert_eq!(payload["type"], "page-replay");
        assert_eq!(payload["last_seq"], 50);
        assert_eq!(
            payload["batches"].as_array().expect("batches array").len(),
            50,
            "every batch still reaches the client"
        );
    }

    /// Delete-wins is applied before the frame is built, so a tombstoned
    /// stroke never reaches the client — and the one frame is what the #324
    /// counters see.
    #[tokio::test]
    async fn the_replay_frame_applies_delete_wins_and_is_counted_once() {
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

        let replay = build_page_replay(PAGE, 2, batches, tombstones);
        assert_eq!(replay.page_id, PAGE);
        assert_eq!(replay.last_seq, 2);
        let surviving: Vec<&str> = replay
            .batches
            .iter()
            .flat_map(|batch| batch.strokes.iter().map(|stroke| stroke.id.as_str()))
            .collect();
        assert_eq!(
            surviving,
            ["kept", "later"],
            "delete-wins filtering must still drop tombstoned strokes"
        );
        assert_eq!(replay.tombstones.len(), 1);

        let store = FakeStore {
            replay: (replay.batches.clone(), replay.tombstones.clone()),
            head_seq: 2,
            ..FakeStore::default()
        };
        let mut sink: Vec<Message> = Vec::new();
        assert!(replay_page(&store, PAGE, &mut sink, 0, Instant::now(), true).await);

        let frames = text_frames(&sink);
        assert_eq!(frames.len(), 1);
        assert!(frames[0].contains(r#""id":"kept""#));
        assert!(!frames[0].contains(r#""id":"erased""#));
    }
}
