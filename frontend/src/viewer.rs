//! The read-only ink viewer for one page: pan/zoom state, the canvas, and the
//! reducers that apply page-channel events to what it shows.

use futures_util::future::{AbortHandle, Abortable};
use js_sys::Reflect;
use leptos::prelude::*;
use leptos::{
    ev,
    leptos_dom::helpers::{
        AnimationFrameRequestHandle, request_animation_frame_with_handle, window_event_listener,
    },
};
use protocol::{PageServerMessage, PageSummary, Paper, StrokeBatch, TombstoneBatch};
use wasm_bindgen::JsCast;
use web_sys::{HtmlCanvasElement, PointerEvent, WheelEvent};

use crate::{library, realtime, render};

/// The viewer opens fully zoomed out; `every_paper_is_visible_at_minimum_canvas_scale`
/// pins that every paper still shows at this scale.
pub(crate) const MIN_CANVAS_SCALE: f64 = 0.08;
const MAX_CANVAS_SCALE: f64 = 4.0;
const WHEEL_ZOOM_STEP: f64 = 1.0163963568148535;

/// The page-channel state the viewer renders. `Copy`, like the signals it holds.
#[derive(Clone, Copy)]
struct PageFeed {
    batches: RwSignal<Vec<StrokeBatch>>,
    status: RwSignal<String>,
    error: RwSignal<Option<String>>,
    last_seq: RwSignal<u64>,
    paper: RwSignal<Paper>,
}

/// Pan and zoom of the viewer canvas, in CSS pixels, plus the drag in progress.
#[derive(Clone, Copy)]
struct PanZoom {
    offset_x: RwSignal<f64>,
    offset_y: RwSignal<f64>,
    scale: RwSignal<f64>,
    dragging: RwSignal<Option<(i32, f64, f64)>>,
}

impl PanZoom {
    fn new() -> Self {
        Self {
            offset_x: RwSignal::new(80.0),
            offset_y: RwSignal::new(80.0),
            scale: RwSignal::new(MIN_CANVAS_SCALE),
            dragging: RwSignal::new(None),
        }
    }

    fn pointer_down(self, event: &PointerEvent) {
        self.dragging.set(Some((
            event.pointer_id(),
            event.client_x() as f64,
            event.client_y() as f64,
        )));
        if let Some(target) = canvas_target(event) {
            let _ = target.set_pointer_capture(event.pointer_id());
        }
    }

    fn pointer_move(self, event: &PointerEvent) {
        if let Some((pointer_id, last_x, last_y)) = self.dragging.get_untracked()
            && pointer_id == event.pointer_id()
        {
            let x = event.client_x() as f64;
            let y = event.client_y() as f64;
            self.offset_x.update(|value| *value += x - last_x);
            self.offset_y.update(|value| *value += y - last_y);
            self.dragging.set(Some((pointer_id, x, y)));
        }
    }

    fn pointer_up(self, event: &PointerEvent) {
        if self
            .dragging
            .get_untracked()
            .is_some_and(|(pointer_id, _, _)| pointer_id == event.pointer_id())
        {
            self.dragging.set(None);
        }
    }

    fn cancel_drag(self) {
        self.dragging.set(None);
    }

    /// Zoom one wheel notch about the cursor, so the ink under it stays put.
    fn wheel(self, event: &WheelEvent) {
        event.prevent_default();
        let old_scale = self.scale.get_untracked();
        let Some(new_scale) = wheel_scale(old_scale, event.delta_y()) else {
            return;
        };
        if let Some(target) = canvas_target(event) {
            let rect = target.get_bounding_client_rect();
            if rect.width() > 0.0 && rect.height() > 0.0 {
                let canvas_x = event.client_x() as f64 - rect.left();
                let canvas_y = event.client_y() as f64 - rect.top();
                self.offset_x.set(zoom_offset(
                    self.offset_x.get_untracked(),
                    canvas_x,
                    old_scale,
                    new_scale,
                ));
                self.offset_y.set(zoom_offset(
                    self.offset_y.get_untracked(),
                    canvas_y,
                    old_scale,
                    new_scale,
                ));
            }
        }
        self.scale.set(new_scale);
    }
}

fn canvas_target(event: &web_sys::MouseEvent) -> Option<HtmlCanvasElement> {
    event
        .target()
        .and_then(|target| target.dyn_into::<HtmlCanvasElement>().ok())
}

/// The scale after one wheel notch (`delta_y < 0` zooms in), or `None` when the
/// scale is already at the limit in that direction.
fn wheel_scale(old_scale: f64, delta_y: f64) -> Option<f64> {
    let factor = if delta_y < 0.0 {
        WHEEL_ZOOM_STEP
    } else {
        1.0 / WHEEL_ZOOM_STEP
    };
    let new_scale = (old_scale * factor).clamp(MIN_CANVAS_SCALE, MAX_CANVAS_SCALE);
    ((new_scale - old_scale).abs() >= f64::EPSILON).then_some(new_scale)
}

/// The offset along one axis that keeps the world point under `canvas` (a CSS
/// position inside the canvas) fixed while the scale changes.
fn zoom_offset(offset: f64, canvas: f64, old_scale: f64, new_scale: f64) -> f64 {
    let world = (canvas - offset) / old_scale;
    canvas - world * new_scale
}

/// Reset the feed and follow the page channel until the owning effect is
/// cleaned up, which aborts the loop.
fn follow_page_channel(page_id: String, initial_paper: Paper, feed: PageFeed) {
    feed.batches.set(Vec::new());
    feed.error.set(None);
    feed.status.set("Connecting".to_string());
    feed.last_seq.set(0);
    feed.paper.set(initial_paper);
    let (abort_handle, abort_registration) = AbortHandle::new_pair();
    on_cleanup(move || abort_handle.abort());
    wasm_bindgen_futures::spawn_local(async move {
        if let Ok(Err(error)) = Abortable::new(
            realtime::page_realtime_loop(
                page_id,
                feed.batches,
                feed.status,
                feed.error,
                feed.last_seq,
                feed.paper,
            ),
            abort_registration,
        )
        .await
        {
            feed.error.set(Some(error));
            feed.status.set("Disconnected".to_string());
        }
    });
}

#[component]
pub(crate) fn InkViewer(page: PageSummary, on_close: Callback<()>) -> impl IntoView {
    let canvas = NodeRef::<leptos::html::Canvas>::new();
    // Seeded from the library listing so the first frame is not blank, then
    // superseded by the authoritative value `Welcome` carries.
    let feed = PageFeed {
        batches: RwSignal::new(Vec::new()),
        status: RwSignal::new("Connecting".to_string()),
        error: RwSignal::new(None),
        last_seq: RwSignal::new(0),
        paper: RwSignal::new(page.paper),
    };
    let pan_zoom = PanZoom::new();
    let canvas_resize_tick = RwSignal::new(0_u64);
    let initial_paper = page.paper;
    let page_id = page.id.clone();
    let page_title = library::page_display_title(&page);

    Effect::new(move |_| follow_page_channel(page_id.clone(), initial_paper, feed));

    // Changes only *request* a paint; the paint itself runs once per animation
    // frame. A pointer or wheel device can report several events per frame and
    // a replay delivers batches back to back, and painting for each of them
    // redraws the whole page for frames nobody sees.
    let pending_frame = StoredValue::new(None::<AnimationFrameRequestHandle>);
    Effect::new(move |_| {
        feed.batches.track();
        feed.paper.track();
        pan_zoom.offset_x.track();
        pan_zoom.offset_y.track();
        pan_zoom.scale.track();
        canvas_resize_tick.track();
        canvas.track();
        if pending_frame.with_value(Option::is_some) {
            return;
        }
        let handle = request_animation_frame_with_handle(move || {
            pending_frame.set_value(None);
            paint(canvas, feed, pan_zoom);
        });
        pending_frame.set_value(handle.ok());
    });
    on_cleanup(move || {
        if let Some(handle) = pending_frame.try_update_value(Option::take).flatten() {
            handle.cancel();
        }
    });

    Effect::new(move |_| {
        let resize_handle = window_event_listener(ev::resize, move |_| {
            canvas_resize_tick.update(|tick| *tick = tick.wrapping_add(1));
        });
        on_cleanup(move || resize_handle.remove());
    });

    view! {
        <section class="canvas-shell" aria-label="Open page">
            // The same `.top-bar` the library wears, carrying the back arrow and
            // the page name instead of the menu and the brand.
            <header class="top-bar">
                <div class="bar-left">
                    <button class="icon-button" aria-label="Back to library" on:click=move |_| on_close.run(())>
                        <span aria-hidden="true">"←"</span>
                    </button>
                    <h2 class="page-name">{page_title}</h2>
                </div>
                <div class="bar-right">
                    <span class="live-status" aria-live="polite">
                        {move || format!("{} · seq {}", feed.status.get(), feed.last_seq.get())}
                    </span>
                </div>
            </header>

            {move || feed.error.get().map(|error| view! {
                <p class="alert" role="alert">{error}</p>
            })}

            <div class="canvas-frame">
            <canvas
                node_ref=canvas
                aria-label="Read-only ink canvas"
                data-testid="ink-canvas"
                class="ink-canvas"
                on:pointerdown=move |event: PointerEvent| pan_zoom.pointer_down(&event)
                on:pointermove=move |event: PointerEvent| pan_zoom.pointer_move(&event)
                on:pointerup=move |event: PointerEvent| pan_zoom.pointer_up(&event)
                on:pointercancel=move |_| pan_zoom.cancel_drag()
                on:wheel=move |event: WheelEvent| pan_zoom.wheel(&event)
            />
            </div>
        </section>
    }
}

/// Paint the page as it stands now. Borrows the batches rather than cloning
/// them — a clone copies every point on the page, every frame.
fn paint(canvas: NodeRef<leptos::html::Canvas>, feed: PageFeed, pan_zoom: PanZoom) {
    let Some(canvas) = canvas.get_untracked() else {
        return;
    };
    let performance = web_sys::window().and_then(|window| window.performance());
    let started = performance.as_ref().map(|performance| performance.now());
    feed.batches.with_untracked(|batches| {
        render::draw_canvas(
            &canvas,
            batches,
            feed.paper.get_untracked(),
            pan_zoom.offset_x.get_untracked(),
            pan_zoom.offset_y.get_untracked(),
            pan_zoom.scale.get_untracked(),
        );
    });
    if let (Some(performance), Some(started)) = (performance, started) {
        mark_ink_drawn(performance.now() - started);
    }
}

/// Pure reducer for the viewer's paper. The page channel is authoritative:
/// `Welcome` carries it on every (re)connect and `PaperChanged` carries each
/// change, which is what self-corrects a `PageSummary.paper` that went stale in
/// an already-open library. Every other event leaves it untouched.
fn next_viewer_paper(current: Paper, event: &PageServerMessage) -> Paper {
    match event {
        PageServerMessage::Welcome { paper, .. }
        | PageServerMessage::PaperChanged { paper, .. } => *paper,
        _ => current,
    }
}

pub(crate) fn apply_page_event(
    event: PageServerMessage,
    batches: RwSignal<Vec<StrokeBatch>>,
    viewer_status: RwSignal<String>,
    viewer_error: RwSignal<Option<String>>,
    last_seq: RwSignal<u64>,
    paper: RwSignal<Paper>,
) {
    let next_paper = next_viewer_paper(paper.get_untracked(), &event);
    if next_paper != paper.get_untracked() {
        paper.set(next_paper);
    }
    match event {
        PageServerMessage::Welcome { last_seq: seq, .. } => {
            viewer_status.set("Connected".to_string());
            last_seq.set(seq.min(last_seq.get_untracked()));
        }
        PageServerMessage::StrokeBatch(batch) => {
            let seq = batch.seq;
            batches.update(|items| insert_batch(items, batch));
            last_seq.update(|current| *current = (*current).max(seq));
            viewer_status.set("Live".to_string());
            viewer_error.set(None);
            mark_ink_applied(seq);
        }
        // The whole replay in one frame (#323), carrying the `last_seq` that
        // used to arrive as `synced`. Its batches are *merged*, not assigned:
        // a reconnect subscribes from the viewer's cursor, so the frame holds
        // only the batches after it and everything already on screen must
        // survive. Its tombstones then apply to the whole page, for the same
        // reason — they may erase ink from a batch this frame did not carry.
        PageServerMessage::PageReplay(replay) => {
            let seq = replay.last_seq;
            batches.update(|items| {
                for batch in replay.batches {
                    insert_batch(items, batch);
                }
                let removed = tombstoned_ids(&replay.tombstones);
                retain_surviving(items, &removed);
            });
            last_seq.update(|current| *current = (*current).max(seq));
            viewer_status.set("Synced".to_string());
            viewer_error.set(None);
            mark_ink_applied(seq);
        }
        PageServerMessage::Synced { last_seq: seq } => {
            last_seq.update(|current| *current = (*current).max(seq));
            viewer_status.set("Synced".to_string());
            viewer_error.set(None);
        }
        PageServerMessage::TombstoneBatch(tombstones) => {
            let removed = tombstoned_ids(std::slice::from_ref(&tombstones));
            batches.update(|items| retain_surviving(items, &removed));
            viewer_status.set("Live".to_string());
        }
        PageServerMessage::Error { message, .. } => {
            viewer_error.set(Some(message));
        }
        // Paper is handled by `next_viewer_paper` above; the SPA is a read-only
        // viewer, so leases never affect what it renders.
        PageServerMessage::PaperChanged { .. }
        | PageServerMessage::LeaseGranted
        | PageServerMessage::LeaseDenied { .. }
        | PageServerMessage::LeaseChanged { .. } => {}
    }
}

/// Every stroke id the given tombstone batches erase.
fn tombstoned_ids(tombstones: &[TombstoneBatch]) -> std::collections::HashSet<&str> {
    tombstones
        .iter()
        .flat_map(|batch| batch.stroke_ids.iter().map(String::as_str))
        .collect()
}

/// Drop every stroke named by a tombstone. Delete wins, so this is applied to
/// a replay too even though the server already filtered it — a cheap pass that
/// keeps the viewer correct if the two ever disagree.
fn retain_surviving(items: &mut [StrokeBatch], removed: &std::collections::HashSet<&str>) {
    for batch in items {
        batch
            .strokes
            .retain(|stroke| !removed.contains(stroke.id.as_str()));
    }
}

/// Insert a batch in `seq` order, replacing any batch already holding its seq.
/// Batches almost always arrive in order, so appending is the common case;
/// only an out-of-order or repeated seq pays for the search.
fn insert_batch(items: &mut Vec<StrokeBatch>, batch: StrokeBatch) {
    match items.binary_search_by_key(&batch.seq, |item| item.seq) {
        Ok(index) => items[index] = batch,
        Err(index) => items.insert(index, batch),
    }
}

/// Expose paint counts and cost to e2e, alongside `__vNoteLastInkAppliedAt`.
fn mark_ink_drawn(duration_ms: f64) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let draws = Reflect::get(&window, &"__vNoteInkDraws".into())
        .ok()
        .and_then(|value| value.as_f64())
        .unwrap_or(0.0);
    let _ = Reflect::set(&window, &"__vNoteInkDraws".into(), &(draws + 1.0).into());
    let _ = Reflect::set(&window, &"__vNoteInkLastDrawMs".into(), &duration_ms.into());
}

fn mark_ink_applied(seq: u64) {
    let Some(window) = web_sys::window() else {
        return;
    };
    if let Some(performance) = window.performance() {
        let _ = Reflect::set(
            &window,
            &"__vNoteLastInkAppliedAt".into(),
            &performance.now().into(),
        );
    }
    let _ = Reflect::set(&window, &"__vNoteLastInkSeq".into(), &(seq as f64).into());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render;

    #[test]
    fn a_wheel_notch_zooms_in_or_out_and_stops_at_the_limits() {
        let zoomed_in = wheel_scale(1.0, -1.0).expect("room to zoom in");
        let zoomed_out = wheel_scale(1.0, 1.0).expect("room to zoom out");
        assert!(zoomed_in > 1.0 && zoomed_out < 1.0);
        assert!(
            (zoomed_in * zoomed_out - 1.0).abs() < 1e-12,
            "in then out is a no-op"
        );
        assert_eq!(wheel_scale(MIN_CANVAS_SCALE, 1.0), None);
        assert_eq!(wheel_scale(MAX_CANVAS_SCALE, -1.0), None);
    }

    #[test]
    fn zooming_keeps_the_world_point_under_the_cursor_fixed() {
        let (offset, cursor, old_scale, new_scale) = (80.0, 300.0, 0.5, 0.75);
        let world_before = (cursor - offset) / old_scale;
        let new_offset = zoom_offset(offset, cursor, old_scale, new_scale);
        let world_after = (cursor - new_offset) / new_scale;
        assert!((world_before - world_after).abs() < 1e-9);
    }

    fn batch(seq: u64, client_batch_id: &str) -> StrokeBatch {
        StrokeBatch {
            seq,
            client_batch_id: client_batch_id.to_string(),
            strokes: Vec::new(),
        }
    }

    #[test]
    fn batches_stay_in_seq_order_and_a_repeated_seq_replaces() {
        let mut items = Vec::new();
        for (seq, id) in [(1, "a"), (2, "b"), (5, "e"), (3, "c"), (2, "b-again")] {
            insert_batch(&mut items, batch(seq, id));
        }
        let order: Vec<_> = items
            .iter()
            .map(|item| (item.seq, item.client_batch_id.as_str()))
            .collect();
        assert_eq!(order, [(1, "a"), (2, "b-again"), (3, "c"), (5, "e")]);
    }

    /// A reconnect subscribes from the viewer's cursor, so its replay frame
    /// holds only the batches after it. Merging — not assigning — is what keeps
    /// the ink already on screen from being wiped by a gap fill.
    #[test]
    fn a_gap_fill_frame_merges_into_the_ink_already_on_screen() {
        let mut items = vec![batch(1, "a"), batch(2, "b")];

        // The frame a `subscribe { from_seq: 2 }` produces.
        for incoming in [batch(3, "c"), batch(4, "d")] {
            insert_batch(&mut items, incoming);
        }

        let order: Vec<_> = items
            .iter()
            .map(|item| (item.seq, item.client_batch_id.as_str()))
            .collect();
        assert_eq!(order, [(1, "a"), (2, "b"), (3, "c"), (4, "d")]);
    }

    /// A replay frame is trusted for order but not for delete-wins: the viewer
    /// re-applies the tombstones it carries, so an erased stroke can never be
    /// rendered even if the server and client ever disagree (#323).
    #[test]
    fn a_replay_frames_tombstones_remove_its_own_strokes() {
        let stroke = |id: &str| protocol::Stroke {
            id: id.to_string(),
            style: protocol::StrokeStyle::default_solid_round(),
            points: Vec::new(),
        };
        let mut items = vec![
            StrokeBatch {
                seq: 1,
                client_batch_id: "a".to_string(),
                strokes: vec![stroke("kept"), stroke("erased")],
            },
            StrokeBatch {
                seq: 2,
                client_batch_id: "b".to_string(),
                strokes: vec![stroke("later")],
            },
        ];
        let tombstones = vec![TombstoneBatch {
            revision: 1,
            client_mutation_id: "erase_1".to_string(),
            stroke_ids: vec!["erased".to_string()],
        }];

        let removed = tombstoned_ids(&tombstones);
        retain_surviving(&mut items, &removed);

        let surviving: Vec<&str> = items
            .iter()
            .flat_map(|batch| batch.strokes.iter().map(|stroke| stroke.id.as_str()))
            .collect();
        assert_eq!(surviving, ["kept", "later"]);
    }

    /// The page channel is authoritative for paper; nothing else changes it.
    #[test]
    fn next_viewer_paper_follows_the_page_channel() {
        let welcome = PageServerMessage::Welcome {
            session_id: "session_1".to_string(),
            last_seq: 0,
            lease_holder: None,
            paper: Paper::SquaredLarge,
        };
        assert_eq!(
            next_viewer_paper(Paper::None, &welcome),
            Paper::SquaredLarge,
            "welcome carries the authoritative value on every reconnect"
        );

        let changed = PageServerMessage::PaperChanged {
            paper: Paper::RuledWide,
            revision: 7,
        };
        assert_eq!(
            next_viewer_paper(Paper::SquaredLarge, &changed),
            Paper::RuledWide
        );

        // A change back to blank is honoured, not treated as "no value".
        let cleared = PageServerMessage::PaperChanged {
            paper: Paper::None,
            revision: 8,
        };
        assert_eq!(next_viewer_paper(Paper::RuledWide, &cleared), Paper::None);

        // Every other event leaves paper untouched.
        for event in [
            PageServerMessage::Synced { last_seq: 3 },
            PageServerMessage::LeaseGranted,
            PageServerMessage::LeaseDenied {
                holder: "other".to_string(),
            },
            PageServerMessage::LeaseChanged { holder: None },
            PageServerMessage::Error {
                code: "paper_failed".to_string(),
                message: "nope".to_string(),
                client_mutation_id: None,
            },
        ] {
            assert_eq!(
                next_viewer_paper(Paper::RuledNarrow, &event),
                Paper::RuledNarrow
            );
        }
    }

    /// Every paper is now visible at the SPA's default (minimum) zoom.
    ///
    /// The viewer always opens fully zoomed out and has no fit-to-content
    /// logic, so when the finest pitch was 32 world units the graded cull hid
    /// small squares and narrow rules until the user zoomed in — paper that
    /// looked broken on open. At the current pitches the finest family is
    /// 96 * 0.08 = 7.68 device px against a 4.0 floor, so nothing is culled on
    /// arrival.
    #[test]
    fn every_paper_is_visible_at_minimum_canvas_scale() {
        let visible = |paper: Paper, scale: f64| {
            let viewport = render::world_viewport(1200.0, 800.0, 0.0, 0.0, scale);
            !protocol::paper_marks(paper, &viewport).is_empty()
        };
        for paper in Paper::ALL {
            if paper == Paper::None {
                continue;
            }
            assert!(
                visible(paper, MIN_CANVAS_SCALE),
                "{} should be visible on open",
                paper.wire_value()
            );
        }

        // The cull still exists — it just takes a much harder zoom-out to reach,
        // and it is still graded by pitch when it does.
        assert!(!visible(Paper::SquaredSmall, 0.035));
        assert!(visible(Paper::SquaredLarge, 0.035));
        // The margin is never culled, so a margin paper still shows one line
        // even when its rules are gone.
        assert!(visible(Paper::RuledMarginNarrow, 0.0001));
    }
}
