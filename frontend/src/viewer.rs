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

#[component]
pub(crate) fn InkViewer(page: PageSummary, on_close: Callback<()>) -> impl IntoView {
    let canvas = NodeRef::<leptos::html::Canvas>::new();
    let batches = RwSignal::new(Vec::<StrokeBatch>::new());
    let viewer_error = RwSignal::new(None::<String>);
    let viewer_status = RwSignal::new("Connecting".to_string());
    let last_seq = RwSignal::new(0_u64);
    let offset_x = RwSignal::new(80.0_f64);
    let offset_y = RwSignal::new(80.0_f64);
    let scale = RwSignal::new(MIN_CANVAS_SCALE);
    let dragging = RwSignal::new(None::<(i32, f64, f64)>);
    let canvas_resize_tick = RwSignal::new(0_u64);
    // Seeded from the library listing so the first frame is not blank, then
    // superseded by the authoritative value `Welcome` carries.
    let paper = RwSignal::new(page.paper);
    let initial_paper = page.paper;
    let page_id = page.id.clone();
    let page_title = library::page_display_title(&page);

    Effect::new(move |_| {
        batches.set(Vec::new());
        viewer_error.set(None);
        viewer_status.set("Connecting".to_string());
        last_seq.set(0);
        paper.set(initial_paper);
        let page_id = page_id.clone();
        let (abort_handle, abort_registration) = AbortHandle::new_pair();
        on_cleanup(move || abort_handle.abort());
        wasm_bindgen_futures::spawn_local(async move {
            if let Ok(Err(error)) = Abortable::new(
                realtime::page_realtime_loop(
                    page_id,
                    batches,
                    viewer_status,
                    viewer_error,
                    last_seq,
                    paper,
                ),
                abort_registration,
            )
            .await
            {
                viewer_error.set(Some(error));
                viewer_status.set("Disconnected".to_string());
            }
        });
    });

    Effect::new(move |_| {
        batches.track();
        paper.track();
        offset_x.track();
        offset_y.track();
        scale.track();
        canvas_resize_tick.track();
        if let Some(canvas) = canvas.get() {
            render::draw_canvas(
                &canvas,
                &batches.get_untracked(),
                paper.get_untracked(),
                offset_x.get_untracked(),
                offset_y.get_untracked(),
                scale.get_untracked(),
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
                    {move || format!("{} · seq {}", viewer_status.get(), last_seq.get())}
                </span>
            </div>

            {move || viewer_error.get().map(|error| view! {
                <p class="alert" role="alert">{error}</p>
            })}

            <div class="canvas-frame">
            <canvas
                node_ref=canvas
                aria-label="Read-only ink canvas"
                data-testid="ink-canvas"
                class="ink-canvas"
                on:pointerdown=move |event: PointerEvent| {
                    dragging.set(Some((event.pointer_id(), event.client_x() as f64, event.client_y() as f64)));
                    if let Some(target) = event.target().and_then(|target| target.dyn_into::<HtmlCanvasElement>().ok()) {
                        let _ = target.set_pointer_capture(event.pointer_id());
                    }
                }
                on:pointermove=move |event: PointerEvent| {
                    if let Some((pointer_id, last_x, last_y)) = dragging.get_untracked()
                        && pointer_id == event.pointer_id()
                    {
                        let x = event.client_x() as f64;
                        let y = event.client_y() as f64;
                        offset_x.update(|value| *value += x - last_x);
                        offset_y.update(|value| *value += y - last_y);
                        dragging.set(Some((pointer_id, x, y)));
                    }
                }
                on:pointerup=move |event: PointerEvent| {
                    if dragging
                        .get_untracked()
                        .is_some_and(|(pointer_id, _, _)| pointer_id == event.pointer_id())
                    {
                        dragging.set(None);
                    }
                }
                on:pointercancel=move |_| dragging.set(None)
                on:wheel=move |event: WheelEvent| {
                    event.prevent_default();
                    let factor = if event.delta_y() < 0.0 { WHEEL_ZOOM_STEP } else { 1.0 / WHEEL_ZOOM_STEP };
                    let old_scale = scale.get_untracked();
                    let new_scale = (old_scale * factor).clamp(MIN_CANVAS_SCALE, MAX_CANVAS_SCALE);
                    if (new_scale - old_scale).abs() < f64::EPSILON {
                        return;
                    }

                    if let Some(target) = event.target().and_then(|target| target.dyn_into::<HtmlCanvasElement>().ok()) {
                        let rect = target.get_bounding_client_rect();
                        let rect_width = rect.width();
                        let rect_height = rect.height();
                        if rect_width > 0.0 && rect_height > 0.0 {
                            let canvas_x = event.client_x() as f64 - rect.left();
                            let canvas_y = event.client_y() as f64 - rect.top();
                            let world_x = (canvas_x - offset_x.get_untracked()) / old_scale;
                            let world_y = (canvas_y - offset_y.get_untracked()) / old_scale;
                            offset_x.set(canvas_x - world_x * new_scale);
                            offset_y.set(canvas_y - world_y * new_scale);
                        }
                    }
                    scale.set(new_scale);
                }
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
