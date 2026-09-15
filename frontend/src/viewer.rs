//! The read-only ink viewer for one page: pan/zoom state, the canvas, and the
//! reducers that apply page-channel events to what it shows.

use futures_util::future::{AbortHandle, Abortable};
use js_sys::Reflect;
use leptos::prelude::*;
use leptos::{ev, leptos_dom::helpers::window_event_listener};
use protocol::{PageServerMessage, PageSummary, Paper, StrokeBatch};
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

    Effect::new(move |_| {
        feed.batches.track();
        feed.paper.track();
        pan_zoom.offset_x.track();
        pan_zoom.offset_y.track();
        pan_zoom.scale.track();
        canvas_resize_tick.track();
        if let Some(canvas) = canvas.get() {
            render::draw_canvas(
                &canvas,
                &feed.batches.get_untracked(),
                feed.paper.get_untracked(),
                pan_zoom.offset_x.get_untracked(),
                pan_zoom.offset_y.get_untracked(),
                pan_zoom.scale.get_untracked(),
            );
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
            <div class="canvas-header">
                <button class="button secondary" on:click=move |_| on_close.run(())>"Back"</button>
                <h2 class="canvas-title">{page_title}</h2>
                <span class="live-status" aria-live="polite">
                    {move || format!("{} · seq {}", feed.status.get(), feed.last_seq.get())}
                </span>
            </div>

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
            batches.update(|items| {
                items.retain(|existing| existing.seq != seq);
                items.push(batch);
                items.sort_by_key(|item| item.seq);
            });
            last_seq.update(|current| *current = (*current).max(seq));
            viewer_status.set("Live".to_string());
            viewer_error.set(None);
            mark_ink_applied(seq);
        }
        PageServerMessage::Synced { last_seq: seq } => {
            last_seq.update(|current| *current = (*current).max(seq));
            viewer_status.set("Synced".to_string());
            viewer_error.set(None);
        }
        PageServerMessage::TombstoneBatch(tombstones) => {
            let removed: std::collections::HashSet<_> = tombstones.stroke_ids.into_iter().collect();
            batches.update(|items| {
                for batch in items {
                    batch.strokes.retain(|stroke| !removed.contains(&stroke.id));
                }
            });
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
