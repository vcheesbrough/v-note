use futures_util::{
    future::{AbortHandle, Abortable},
    SinkExt, StreamExt,
};
use gloo_net::http::Request;
use gloo_net::websocket::{futures::WebSocket, Message};
use gloo_timers::future::TimeoutFuture;
use js_sys::{Date, Reflect};
use leptos::prelude::*;
use leptos::{ev, leptos_dom::helpers::window_event_listener};
use protocol::{
    paper_mark_device_width, visit_paper_marks, LibraryEvent, ListPagesResponse, MeResponse,
    MetaResponse, PageServerMessage, PageSummary, Paper, RealtimeTicketResponse, Stroke,
    StrokeBatch, ThumbnailMetadata, WorldViewport,
};
use wasm_bindgen::JsCast;
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement, PointerEvent, WheelEvent};

/// CI release tag (`V_NOTE_RELEASE`) when present, else the cargo version for local builds.
fn release_version() -> &'static str {
    match option_env!("V_NOTE_RELEASE") {
        Some(v) if !v.is_empty() => v,
        _ => env!("CARGO_PKG_VERSION"),
    }
}

fn apk_download_filename() -> String {
    format!("v-note-{}-dev-debug.apk", release_version())
}

fn apk_download_url() -> String {
    format!("/dl/apk?release={}", release_version())
}

const UNTITLED_PAGE: &str = "Untitled page";
const REQUEST_ID_HEADER: &str = "X-Request-Id";

fn page_has_title(page: &PageSummary) -> bool {
    let title = page.title.trim();
    !title.is_empty() && title != UNTITLED_PAGE
}

fn page_display_title(page: &PageSummary) -> String {
    if page_has_title(page) {
        page.title.clone()
    } else {
        approximate_relative_datetime(&page.updated_at)
    }
}

fn request_id() -> String {
    let now = Date::now().round() as u64;
    let random = (js_sys::Math::random() * 1_000_000_000_000.0).round() as u64;
    format!("spa_{now:x}_{random:x}")
}

fn compact_datetime(value: &str) -> String {
    value
        .trim_end_matches('Z')
        .replace('T', " ")
        .chars()
        .take(16)
        .collect()
}

fn approximate_relative_datetime(value: &str) -> String {
    let then = Date::parse(value);
    if then.is_nan() {
        return compact_datetime(value);
    }

    let elapsed_seconds = ((Date::now() - then) / 1000.0).max(0.0).round() as u64;
    let (amount, unit) = match elapsed_seconds {
        0..=89 => return "just now".to_string(),
        90..=5_399 => ((elapsed_seconds + 30) / 60, "minute"),
        5_400..=129_599 => ((elapsed_seconds + 1_800) / 3_600, "hour"),
        129_600..=3_887_999 => ((elapsed_seconds + 43_200) / 86_400, "day"),
        _ => ((elapsed_seconds + 1_296_000) / 2_592_000, "month"),
    };
    let suffix = if amount == 1 { "" } else { "s" };
    format!("{amount} {unit}{suffix} ago")
}

#[component]
fn App() -> impl IntoView {
    let meta = RwSignal::new(None::<Result<MetaResponse, String>>);
    let me = RwSignal::new(None::<Option<Result<MeResponse, u16>>>);
    let pages = RwSignal::new(Vec::<PageSummary>::new());
    let selected_page = RwSignal::new(None::<PageSummary>);
    let library_error = RwSignal::new(None::<String>);

    Effect::new(move |_| {
        wasm_bindgen_futures::spawn_local(async move {
            let meta_result = async {
                let response = Request::get("/api/meta")
                    .header(REQUEST_ID_HEADER, &request_id())
                    .send()
                    .await
                    .map_err(|error| format!("request failed: {error}"))?;
                response
                    .json::<MetaResponse>()
                    .await
                    .map_err(|error| format!("invalid response JSON: {error}"))
            }
            .await;
            meta.set(Some(meta_result));

            let me_result = Request::get("/api/me")
                .header(REQUEST_ID_HEADER, &request_id())
                .send()
                .await;
            me.set(Some(match me_result {
                Ok(response) if response.ok() => match response.json::<MeResponse>().await {
                    Ok(profile) => Some(Ok(profile)),
                    Err(_) => Some(Err(500)),
                },
                Ok(response) => Some(Err(response.status())),
                Err(_) => Some(Err(0)),
            }));
        });
    });

    Effect::new(move |_| {
        if matches!(me.get(), Some(Some(Ok(_)))) {
            wasm_bindgen_futures::spawn_local(async move {
                if let Err(error) = load_pages(pages, library_error).await {
                    library_error.set(Some(error));
                }
            });
            wasm_bindgen_futures::spawn_local(library_realtime_loop(
                pages,
                selected_page,
                library_error,
            ));
        }
    });

    let viewer_open = move || selected_page.get().is_some();

    view! {
        <main class=move || if viewer_open() { "app-shell viewer-mode" } else { "app-shell" }>
            {move || if !viewer_open() {
                view! {
            <header class="top-bar">
                <h1 class="brand">"v-note"</h1>
                {move || match me.get() {
                    None => view! { <span class="muted">"Checking session…"</span> }.into_any(),
                    Some(Some(Ok(profile))) => view! {
                        <div class="session-actions desktop-session-actions">
                            <span class="identity">{profile.email.clone().unwrap_or_else(|| profile.sub.clone())}</span>
                            <a class="button secondary" href="/auth/logout">"Sign out"</a>
                        </div>
                        <details class="session-menu">
                            <summary aria-label="Open account menu">"☰"</summary>
                            <div class="session-menu-panel">
                                <p class="menu-identity">{profile.email.clone().unwrap_or_else(|| profile.sub.clone())}</p>
                                <a class="menu-link" href="/auth/logout">"Sign out"</a>
                                <a class="menu-link" href=apk_download_url() download=apk_download_filename()>"Download Android app (.apk)"</a>
                            </div>
                        </details>
                    }
                    .into_any(),
                    Some(Some(Err(401))) | Some(Some(Err(403))) => view! {
                        <a class="button primary" href="/auth/login">"Sign in"</a>
                    }
                    .into_any(),
                    Some(Some(Err(_))) => view! {
                        <span class="muted">"Session check failed"</span>
                    }
                    .into_any(),
                    Some(None) => view! { <span class="muted">"Session unavailable"</span> }.into_any(),
                }}
            </header>
                }.into_any()
            } else {
                view! {}.into_any()
            }}

            {move || match me.get() {
                None => view! {
                    <section class="state-panel" aria-label="Loading session">
                        <p class="eyebrow">"Session"</p>
                        <h2 class="section-title">"Checking access"</h2>
                        <p class="muted">"Loading your private v-note session."</p>
                    </section>
                }.into_any(),
                Some(Some(Ok(_))) => view! {
                    {move || match selected_page.get() {
                        Some(page) => view! { <InkViewer page=page on_close=Callback::new(move |_| selected_page.set(None)) /> }.into_any(),
                        None => view! {
                    <section class="library-shell" aria-label="Page library">
                        {move || library_error.get().map(|error| view! {
                            <p class="alert" role="alert">{error}</p>
                        })}

                        {move || if pages.get().is_empty() {
                            view! {
                                <div class="state-panel">
                                    <p class="eyebrow">"No pages"</p>
                                    <h3 class="section-title">"Start with a blank ink page"</h3>
                                    <p class="muted">"Create a page, then open it on Android to write with the S Pen."</p>
                                </div>
                            }.into_any()
                        } else {
                            view! {
                        <ul class="page-grid">
                            <For
                                each=move || {
                                    library_error.track();
                                    pages.get()
                                }
                                // `updated_at` is in the key so a page-updated event (which only
                                // bumps the timestamp) rebuilds the row, refreshing the captured
                                // "Updated …" age label and the click-handler's page snapshot.
                                key=|page| (page.id.clone(), page.updated_at.clone(), thumbnail_key(&page.thumbnail))
                                children=move |page| {
                                    let preview_page = page.clone();
                                    let delete_page_id = page.id.clone();
                                    let display_title = page_has_title(&page).then(|| page.title.clone());
                                    let page_age = approximate_relative_datetime(&page.updated_at);
                                    let open_label = display_title.as_ref().map_or_else(
                                        || "Open page".to_string(),
                                        |title| format!("Open {title}"),
                                    );
                                    let delete_label = display_title.as_ref().map_or_else(
                                        || "Delete page".to_string(),
                                        |title| format!("Delete {title}"),
                                    );
                                    let thumbnail = page.thumbnail.clone();
                                    let thumbnail_unavailable = library_error.get_untracked().is_some();
                                    view! {
                                        <li class="page-tile">
                                            <button class="page-preview-button" aria-label=open_label.clone() on:click=move |_| selected_page.set(Some(preview_page.clone()))>
                                            {match (thumbnail_unavailable, thumbnail) {
                                                (true, _) => view! {
                                                    <div class="page-preview thumbnail-failed" aria-label="Thumbnail unavailable while realtime is disconnected"></div>
                                                }.into_any(),
                                                (false, ThumbnailMetadata::Available { url, .. }) => view! {
                                                    <img class="page-preview" src=url alt="" />
                                                }.into_any(),
                                                (false, ThumbnailMetadata::Generating { .. }) => view! {
                                                    <div class="page-preview thumbnail-generating" aria-label="Thumbnail generating"></div>
                                                }.into_any(),
                                                (false, ThumbnailMetadata::Failed { .. }) => view! {
                                                    <div class="page-preview thumbnail-failed" aria-label="Thumbnail unavailable"></div>
                                                }.into_any(),
                                                (false, ThumbnailMetadata::Empty) => view! {
                                                    <div class="page-preview thumbnail-empty" aria-label="Empty page"></div>
                                                }.into_any(),
                                            }}
                                            {display_title.clone().map(|title| view! {
                                                <span class="page-thumbnail-title">{title}</span>
                                            })}
                                            </button>
                                            <div class="page-tile-footer">
                                            <div class="page-main">
                                                <span class="page-age">{page_age}</span>
                                            </div>
                                            <button class="page-delete" aria-label=delete_label on:click=move |_| {
                                                let page_id = delete_page_id.clone();
                                                if web_sys::window()
                                                    .and_then(|window| window.confirm_with_message("Delete this page permanently?").ok())
                                                    .unwrap_or(false)
                                                {
                                                    wasm_bindgen_futures::spawn_local(async move {
                                                        if let Err(error) = delete_page(page_id, pages, selected_page, library_error).await {
                                                            library_error.set(Some(error));
                                                        }
                                                    });
                                                }
                                            } title="Delete page">
                                                "×"
                                            </button>
                                            </div>
                                        </li>
                                    }
                                }
                            />
                        </ul>
                            }.into_any()
                        }}

                        {move || match meta.get() {
                            None => view! { <p class="muted">"Loading metadata…"</p> }.into_any(),
                            Some(Ok(info)) => view! {
                                <p class="muted">{format!("Version {} · Protocol {}", info.app_version, info.protocol_version)}</p>
                            }
                            .into_any(),
                            Some(Err(error)) => view! {
                                <p class="alert" role="alert">{format!("Failed to load metadata: {error}")}</p>
                            }
                            .into_any(),
                        }}

                        <p class="apk-link"><a class="button secondary" href=apk_download_url() download=apk_download_filename()>"Download Android app (.apk)"</a></p>
                    </section>
                        }.into_any(),
                    }}
                }
                .into_any(),
                Some(Some(Err(401))) | Some(Some(Err(403))) => view! {
                    <section class="auth-panel state-panel">
                        <p class="eyebrow">"Private notes"</p>
                        <h2 class="section-title">"Sign in to continue"</h2>
                        <a class="button primary" href="/auth/login">"Continue to sign in"</a>
                    </section>
                }
                .into_any(),
                _ => view! {
                    <section class="state-panel" role="alert">
                        <p class="eyebrow">"Session"</p>
                        <h2 class="section-title">"Unable to determine authentication state"</h2>
                    </section>
                }
                .into_any(),
            }}
        </main>

        // Version watermark — compile-time build version, rendered on every screen
        // (including the pre-login front screen) with no dependency on /api/meta.
        // CI injects the computed release tag via V_NOTE_RELEASE so this matches the
        // deployed image + /api/meta; local builds fall back to the cargo version.
        <div class="version-watermark">
            {format!("v{}", release_version())}
        </div>
    }
}

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}

#[component]
fn InkViewer(page: PageSummary, on_close: Callback<()>) -> impl IntoView {
    const MIN_CANVAS_SCALE: f64 = 0.08;
    const MAX_CANVAS_SCALE: f64 = 4.0;
    const WHEEL_ZOOM_STEP: f64 = 1.0163963568148535;

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
    let page_title = page_display_title(&page);

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
                page_realtime_loop(
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
            draw_canvas(
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
                    if let Some((pointer_id, last_x, last_y)) = dragging.get_untracked() {
                        if pointer_id == event.pointer_id() {
                            let x = event.client_x() as f64;
                            let y = event.client_y() as f64;
                            offset_x.update(|value| *value += x - last_x);
                            offset_y.update(|value| *value += y - last_y);
                            dragging.set(Some((pointer_id, x, y)));
                        }
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

async fn page_realtime_loop(
    page_id: String,
    batches: RwSignal<Vec<StrokeBatch>>,
    viewer_status: RwSignal<String>,
    viewer_error: RwSignal<Option<String>>,
    last_seq: RwSignal<u64>,
    paper: RwSignal<Paper>,
) -> Result<(), String> {
    loop {
        match page_realtime_once(
            &page_id,
            batches,
            viewer_status,
            viewer_error,
            last_seq,
            paper,
        )
        .await
        {
            Ok(()) => TimeoutFuture::new(500).await,
            Err(error) => {
                viewer_error.set(Some(error));
                viewer_status.set("Reconnecting".to_string());
                TimeoutFuture::new(1_000).await;
            }
        }
    }
}

async fn page_realtime_once(
    page_id: &str,
    batches: RwSignal<Vec<StrokeBatch>>,
    viewer_status: RwSignal<String>,
    viewer_error: RwSignal<Option<String>>,
    last_seq: RwSignal<u64>,
    paper: RwSignal<Paper>,
) -> Result<(), String> {
    let ticket_response = Request::post("/api/realtime-ticket")
        .header(REQUEST_ID_HEADER, &request_id())
        .send()
        .await
        .map_err(|error| format!("page realtime ticket failed: {error}"))?;
    if !ticket_response.ok() {
        return Err(format!(
            "page realtime ticket failed: HTTP {}",
            ticket_response.status()
        ));
    }
    let ticket = ticket_response
        .json::<RealtimeTicketResponse>()
        .await
        .map_err(|error| format!("invalid page realtime ticket: {error}"))?;
    let ws_url = page_realtime_url(page_id, &ticket.ticket)?;
    let socket = WebSocket::open(&ws_url)
        .map_err(|error| format!("opening page realtime socket failed: {error:?}"))?;
    let (mut write, mut read) = socket.split();
    let from_seq = last_seq.get_untracked();
    write
        .send(Message::Text(
            serde_json::json!({ "type": "subscribe", "from_seq": from_seq }).to_string(),
        ))
        .await
        .map_err(|error| format!("page subscribe failed: {error:?}"))?;
    viewer_status.set("Subscribing".to_string());

    while let Some(message) = read.next().await {
        let message = message.map_err(|error| format!("page realtime socket failed: {error:?}"))?;
        if let Message::Text(text) = message {
            let event: PageServerMessage = serde_json::from_str(&text)
                .map_err(|error| format!("invalid page realtime event: {error}"))?;
            apply_page_event(event, batches, viewer_status, viewer_error, last_seq, paper);
        }
    }
    Err("page realtime socket closed".to_string())
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

fn apply_page_event(
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

/// The world-space rectangle currently on screen — the exact inverse of the
/// `world * scale + offset` mapping the stroke loop applies, so paper is
/// enumerated for precisely the area being painted.
///
/// `scale` here is CSS px per world unit: the DPR transform is applied outside
/// this function, so on a 2× display the *physical* pitch is double the cull
/// threshold. That is conservative — it can only ever cull too early, never too
/// late — and is deliberately left uncorrected.
fn world_viewport(
    css_width: f64,
    css_height: f64,
    offset_x: f64,
    offset_y: f64,
    scale: f64,
) -> WorldViewport {
    WorldViewport::new(
        (0.0 - offset_x) / scale,
        (0.0 - offset_y) / scale,
        (css_width - offset_x) / scale,
        (css_height - offset_y) / scale,
        scale,
    )
}

/// Paint the page's paper behind the ink, under the same world→CSS mapping the
/// stroke loop uses, so it stays locked to the ink through pan and zoom.
fn draw_paper(
    context: &CanvasRenderingContext2d,
    paper: Paper,
    viewport: &WorldViewport,
    offset_x: f64,
    offset_y: f64,
    scale: f64,
) {
    if paper == Paper::None {
        return;
    }
    // Butt caps: a round cap would bulge each line's ends past the viewport edge.
    // Restored to "round" for ink by the caller.
    context.set_line_cap("butt");

    // Batch by style instead of stroking each mark on its own. `visit_paper_marks`
    // yields marks grouped by kind, and rules and columns share a colour *and* a
    // width, so a whole grid collapses to one `begin_path` + one style pair + one
    // `stroke()`, with the margin as a second batch. Disjoint `move_to`/`line_to`
    // pairs are separate subpaths of that single path — this is exactly what
    // Canvas2D batching is for, and it turns dozens of JS/WASM boundary crossings
    // per frame into about three.
    let mut batch: Option<(&'static str, f64)> = None;
    visit_paper_marks(paper, viewport, |mark| {
        let style = (mark.kind.color(), mark.world_width());
        if batch != Some(style) {
            if batch.is_some() {
                context.stroke();
            }
            context.begin_path();
            context.set_stroke_style_str(style.0);
            context.set_line_width(paper_mark_device_width(style.1, scale));
            batch = Some(style);
        }
        if mark.kind.is_horizontal() {
            let y = mark.position * scale + offset_y;
            context.move_to(viewport.min_x * scale + offset_x, y);
            context.line_to(viewport.max_x * scale + offset_x, y);
        } else {
            let x = mark.position * scale + offset_x;
            context.move_to(x, viewport.min_y * scale + offset_y);
            context.line_to(x, viewport.max_y * scale + offset_y);
        }
    });
    if batch.is_some() {
        context.stroke();
    }
}

fn draw_canvas(
    canvas: &HtmlCanvasElement,
    batches: &[StrokeBatch],
    paper: Paper,
    offset_x: f64,
    offset_y: f64,
    scale: f64,
) {
    let rect = canvas.get_bounding_client_rect();
    let css_width = rect.width().max(1.0);
    let css_height = rect.height().max(1.0);
    let dpr = web_sys::window()
        .map(|window| window.device_pixel_ratio())
        .unwrap_or(1.0)
        .max(1.0);
    let backing_width = (css_width * dpr).round() as u32;
    let backing_height = (css_height * dpr).round() as u32;
    if canvas.width() != backing_width {
        canvas.set_width(backing_width);
    }
    if canvas.height() != backing_height {
        canvas.set_height(backing_height);
    }

    let Ok(Some(context)) = canvas.get_context("2d") else {
        return;
    };
    let Ok(context) = context.dyn_into::<CanvasRenderingContext2d>() else {
        return;
    };
    let _ = context.set_transform(dpr, 0.0, 0.0, dpr, 0.0, 0.0);
    context.set_fill_style_str("#ffffff");
    context.fill_rect(0.0, 0.0, css_width, css_height);

    // Paper goes on after the white fill and before every stroke, so it can
    // never overpaint ink.
    let viewport = world_viewport(css_width, css_height, offset_x, offset_y, scale);
    draw_paper(&context, paper, &viewport, offset_x, offset_y, scale);

    context.set_line_cap("round");
    context.set_line_join("round");

    for stroke in batches
        .iter()
        .flat_map(|batch| batch.strokes.iter())
        .filter(|stroke| stroke.points.len() >= 2)
    {
        draw_stroke(&context, stroke, offset_x, offset_y, scale);
    }
}

const MIN_RENDERED_STROKE_WIDTH: f64 = 0.75;

fn draw_stroke(
    context: &CanvasRenderingContext2d,
    stroke: &Stroke,
    offset_x: f64,
    offset_y: f64,
    scale: f64,
) {
    context.set_stroke_style_str(&stroke.style.parameters.color);

    // Pressure-modulated (v2) strokes replay as per-segment variable-width
    // paths; v1 keeps the single constant-width path below byte-identical.
    if stroke.style.is_pressure_sensitive() {
        draw_pressure_stroke(context, stroke, offset_x, offset_y, scale);
        return;
    }

    context.begin_path();
    context.set_line_width((stroke.style.parameters.width * scale).max(MIN_RENDERED_STROKE_WIDTH));
    if let Some(first) = stroke.points.first() {
        context.move_to(first.x * scale + offset_x, first.y * scale + offset_y);
        for point in stroke.points.iter().skip(1) {
            context.line_to(point.x * scale + offset_x, point.y * scale + offset_y);
        }
    }
    context.stroke();
}

/// Replay one v2 stroke as a chain of round-capped segments, each stroked at the
/// mean of its endpoints' pressure widths (the shared curve in `protocol`).
/// Round caps overlap consecutive segments so joins stay continuous.
fn draw_pressure_stroke(
    context: &CanvasRenderingContext2d,
    stroke: &Stroke,
    offset_x: f64,
    offset_y: f64,
    scale: f64,
) {
    let style = &stroke.style;
    for pair in stroke.points.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        let width =
            (style.rendered_width(a.pressure) + style.rendered_width(b.pressure)) / 2.0 * scale;
        context.begin_path();
        context.set_line_width(width.max(MIN_RENDERED_STROKE_WIDTH));
        context.move_to(a.x * scale + offset_x, a.y * scale + offset_y);
        context.line_to(b.x * scale + offset_x, b.y * scale + offset_y);
        context.stroke();
    }
}

async fn load_pages(
    pages: RwSignal<Vec<PageSummary>>,
    library_error: RwSignal<Option<String>>,
) -> Result<(), String> {
    let response = Request::get("/api/pages")
        .header(REQUEST_ID_HEADER, &request_id())
        .send()
        .await
        .map_err(|error| format!("loading pages failed: {error}"))?;
    if !response.ok() {
        return Err(format!("loading pages failed: HTTP {}", response.status()));
    }
    let body = response
        .json::<ListPagesResponse>()
        .await
        .map_err(|error| format!("invalid pages response: {error}"))?;
    pages.set(body.pages);
    library_error.set(None);
    Ok(())
}

async fn delete_page(
    page_id: String,
    pages: RwSignal<Vec<PageSummary>>,
    selected_page: RwSignal<Option<PageSummary>>,
    library_error: RwSignal<Option<String>>,
) -> Result<(), String> {
    let response = Request::delete(&format!("/api/pages/{page_id}"))
        .header(REQUEST_ID_HEADER, &request_id())
        .send()
        .await
        .map_err(|error| format!("deleting page failed: {error}"))?;
    if !response.ok() {
        return Err(format!("deleting page failed: HTTP {}", response.status()));
    }
    remove_page(pages, selected_page, &page_id);
    library_error.set(None);
    Ok(())
}

async fn library_realtime_loop(
    pages: RwSignal<Vec<PageSummary>>,
    selected_page: RwSignal<Option<PageSummary>>,
    library_error: RwSignal<Option<String>>,
) {
    loop {
        if let Err(error) = library_realtime_once(pages, selected_page, library_error).await {
            library_error.set(Some(error));
            TimeoutFuture::new(1_000).await;
        }
    }
}

async fn library_realtime_once(
    pages: RwSignal<Vec<PageSummary>>,
    selected_page: RwSignal<Option<PageSummary>>,
    library_error: RwSignal<Option<String>>,
) -> Result<(), String> {
    let ticket_response = Request::post("/api/realtime-ticket")
        .header(REQUEST_ID_HEADER, &request_id())
        .send()
        .await
        .map_err(|error| format!("realtime ticket failed: {error}"))?;
    if !ticket_response.ok() {
        return Err(format!(
            "realtime ticket failed: HTTP {}",
            ticket_response.status()
        ));
    }
    let ticket = ticket_response
        .json::<RealtimeTicketResponse>()
        .await
        .map_err(|error| format!("invalid realtime ticket: {error}"))?;

    load_pages(pages, library_error).await?;

    let ws_url = realtime_url(&ticket.ticket)?;
    let mut socket = WebSocket::open(&ws_url)
        .map_err(|error| format!("opening realtime socket failed: {error:?}"))?;
    while let Some(message) = socket.next().await {
        let message = message.map_err(|error| format!("realtime socket failed: {error:?}"))?;
        if let Message::Text(text) = message {
            let event: LibraryEvent = serde_json::from_str(&text)
                .map_err(|error| format!("invalid realtime event: {error}"))?;
            apply_event(pages, selected_page, event);
            library_error.set(None);
        }
    }

    Err("realtime socket closed".to_string())
}

fn realtime_url(ticket: &str) -> Result<String, String> {
    websocket_url(&format!("/api/realtime?ticket={ticket}"))
}

fn page_realtime_url(page_id: &str, ticket: &str) -> Result<String, String> {
    websocket_url(&format!("/api/pages/{page_id}/realtime?ticket={ticket}"))
}

fn websocket_url(path_and_query: &str) -> Result<String, String> {
    let window = web_sys::window().ok_or_else(|| "window unavailable".to_string())?;
    let location = window.location();
    let protocol = location
        .protocol()
        .map_err(|_| "location protocol unavailable".to_string())?;
    let host = location
        .host()
        .map_err(|_| "location host unavailable".to_string())?;
    let scheme = if protocol == "https:" { "wss" } else { "ws" };
    Ok(format!("{scheme}://{host}{path_and_query}"))
}

fn apply_event(
    pages: RwSignal<Vec<PageSummary>>,
    selected_page: RwSignal<Option<PageSummary>>,
    event: LibraryEvent,
) {
    // Clearing a deleted page's selection is the only cross-signal effect; the
    // list mutation itself is a pure reducer so it can be unit-tested.
    if let LibraryEvent::PageDeleted { page_id } = &event {
        if selected_page
            .get_untracked()
            .as_ref()
            .is_some_and(|page| &page.id == page_id)
        {
            selected_page.set(None);
        }
    }
    pages.update(|items| apply_library_event_to_pages(items, event));
}

/// Apply a library event to the in-memory page list, keeping recent-first
/// (`updated_at` DESC) order. Pure over the vector so it is unit-testable
/// without a reactive runtime.
fn apply_library_event_to_pages(items: &mut Vec<PageSummary>, event: LibraryEvent) {
    match event {
        LibraryEvent::PageCreated { page } => {
            items.retain(|existing| existing.id != page.id);
            items.push(page);
            sort_pages_recent_first(items);
        }
        LibraryEvent::PageDeleted { page_id } => items.retain(|page| page.id != page_id),
        LibraryEvent::PageThumbnailUpdated { page_id, thumbnail } => {
            if let Some(page) = items.iter_mut().find(|page| page.id == page_id) {
                if thumbnail_seq(&thumbnail) >= thumbnail_seq(&page.thumbnail) {
                    page.thumbnail = thumbnail;
                }
            }
        }
        LibraryEvent::PageUpdated {
            page_id,
            updated_at,
        } => {
            let Some(index) = items.iter().position(|page| page.id == page_id) else {
                return;
            };
            // Ignore a stale/duplicate timestamp so re-sort stays idempotent.
            if updated_at <= items[index].updated_at {
                return;
            }
            items[index].updated_at = updated_at;
            sort_pages_recent_first(items);
        }
    }
}

fn sort_pages_recent_first(items: &mut [PageSummary]) {
    items.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
}

fn thumbnail_seq(thumbnail: &ThumbnailMetadata) -> u64 {
    match thumbnail {
        ThumbnailMetadata::Empty => 0,
        ThumbnailMetadata::Generating { source_seq }
        | ThumbnailMetadata::Available { source_seq, .. }
        | ThumbnailMetadata::Failed { source_seq } => *source_seq,
    }
}

fn thumbnail_key(thumbnail: &ThumbnailMetadata) -> String {
    match thumbnail {
        ThumbnailMetadata::Empty => "empty".to_string(),
        ThumbnailMetadata::Generating { source_seq } => format!("generating:{source_seq}"),
        ThumbnailMetadata::Available { source_seq, url } => format!("available:{source_seq}:{url}"),
        ThumbnailMetadata::Failed { source_seq } => format!("failed:{source_seq}"),
    }
}

fn remove_page(
    pages: RwSignal<Vec<PageSummary>>,
    selected_page: RwSignal<Option<PageSummary>>,
    page_id: &str,
) {
    pages.update(|items| items.retain(|page| page.id != page_id));
    if selected_page
        .get_untracked()
        .as_ref()
        .is_some_and(|page| page.id == page_id)
    {
        selected_page.set(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(id: &str, updated_at: &str) -> PageSummary {
        PageSummary {
            id: id.to_string(),
            title: id.to_string(),
            created_at: "2026-07-22T00:00:00+00:00".to_string(),
            updated_at: updated_at.to_string(),
            thumbnail: ThumbnailMetadata::Empty,
            paper: Paper::None,
        }
    }

    fn ids(items: &[PageSummary]) -> Vec<&str> {
        items.iter().map(|page| page.id.as_str()).collect()
    }

    #[test]
    fn page_updated_moves_edited_page_to_front() {
        let mut items = vec![
            summary("newer", "2026-07-22T10:00:00+00:00"),
            summary("older", "2026-07-22T09:00:00+00:00"),
        ];
        apply_library_event_to_pages(
            &mut items,
            LibraryEvent::PageUpdated {
                page_id: "older".to_string(),
                updated_at: "2026-07-22T11:00:00+00:00".to_string(),
            },
        );
        assert_eq!(ids(&items), vec!["older", "newer"]);
        assert_eq!(items[0].updated_at, "2026-07-22T11:00:00+00:00");
    }

    #[test]
    fn page_updated_ignores_stale_or_equal_timestamp() {
        let mut items = vec![
            summary("a", "2026-07-22T10:00:00+00:00"),
            summary("b", "2026-07-22T09:00:00+00:00"),
        ];
        // Equal timestamp is a no-op.
        apply_library_event_to_pages(
            &mut items,
            LibraryEvent::PageUpdated {
                page_id: "b".to_string(),
                updated_at: "2026-07-22T09:00:00+00:00".to_string(),
            },
        );
        // Older timestamp is a no-op.
        apply_library_event_to_pages(
            &mut items,
            LibraryEvent::PageUpdated {
                page_id: "b".to_string(),
                updated_at: "2026-07-22T08:00:00+00:00".to_string(),
            },
        );
        assert_eq!(ids(&items), vec!["a", "b"]);
        assert_eq!(items[1].updated_at, "2026-07-22T09:00:00+00:00");
    }

    #[test]
    fn page_updated_for_unknown_page_is_ignored() {
        let mut items = vec![summary("a", "2026-07-22T10:00:00+00:00")];
        apply_library_event_to_pages(
            &mut items,
            LibraryEvent::PageUpdated {
                page_id: "missing".to_string(),
                updated_at: "2026-07-22T12:00:00+00:00".to_string(),
            },
        );
        assert_eq!(ids(&items), vec!["a"]);
        assert_eq!(items[0].updated_at, "2026-07-22T10:00:00+00:00");
    }

    /// `world_viewport` must be the exact inverse of the `world * scale + offset`
    /// mapping the stroke loop applies, or paper drifts against the ink.
    #[test]
    fn world_viewport_inverts_the_ink_transform() {
        let (css_w, css_h, offset_x, offset_y, scale) = (800.0, 600.0, 80.0, -45.0, 0.5);
        let viewport = world_viewport(css_w, css_h, offset_x, offset_y, scale);
        let to_css = |world: f64, offset: f64| world * scale + offset;
        assert!((to_css(viewport.min_x, offset_x) - 0.0).abs() < 1e-9);
        assert!((to_css(viewport.min_y, offset_y) - 0.0).abs() < 1e-9);
        assert!((to_css(viewport.max_x, offset_x) - css_w).abs() < 1e-9);
        assert!((to_css(viewport.max_y, offset_y) - css_h).abs() < 1e-9);
        assert_eq!(viewport.scale, scale);
    }

    /// Panning moves the world window by exactly the inverse pan, so paper stays
    /// locked to the ink rather than sliding under it.
    #[test]
    fn world_viewport_tracks_pan_and_zoom() {
        let base = world_viewport(400.0, 300.0, 0.0, 0.0, 1.0);
        let panned = world_viewport(400.0, 300.0, 100.0, 50.0, 1.0);
        assert!((panned.min_x - (base.min_x - 100.0)).abs() < 1e-9);
        assert!((panned.min_y - (base.min_y - 50.0)).abs() < 1e-9);
        // Zooming in halves the world extent on screen.
        let zoomed = world_viewport(400.0, 300.0, 0.0, 0.0, 2.0);
        assert!(((zoomed.max_x - zoomed.min_x) * 2.0 - (base.max_x - base.min_x)).abs() < 1e-9);
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

    /// A library re-sort must not drop the paper it is not carrying, and a
    /// created page must arrive with the paper it was born with.
    #[test]
    fn library_events_preserve_and_carry_paper() {
        let mut items = vec![summary("a", "2026-07-22T10:00:00+00:00")];
        items[0].paper = Paper::SquaredSmall;
        apply_library_event_to_pages(
            &mut items,
            LibraryEvent::PageUpdated {
                page_id: "a".to_string(),
                updated_at: "2026-07-22T11:00:00+00:00".to_string(),
            },
        );
        assert_eq!(
            items[0].paper,
            Paper::SquaredSmall,
            "re-sort must not blank paper"
        );

        let mut created = summary("b", "2026-07-22T12:00:00+00:00");
        created.paper = Paper::RuledMarginWide;
        apply_library_event_to_pages(&mut items, LibraryEvent::PageCreated { page: created });
        let page = items
            .iter()
            .find(|page| page.id == "b")
            .expect("created page");
        assert_eq!(page.paper, Paper::RuledMarginWide);
    }

    /// At the SPA's default (minimum) zoom the graded cull leaves the coarse
    /// papers visible and hides the fine ones. This is specified behaviour, and
    /// it is why the e2e spec has to zoom in before asserting on narrow rules.
    #[test]
    fn cull_at_minimum_canvas_scale_is_graded() {
        const MIN_CANVAS_SCALE: f64 = 0.08;
        let visible = |paper: Paper| {
            let viewport = world_viewport(1200.0, 800.0, 0.0, 0.0, MIN_CANVAS_SCALE);
            !protocol::paper_marks(paper, &viewport).is_empty()
        };
        assert!(visible(Paper::RuledWide));
        assert!(visible(Paper::SquaredLarge));
        assert!(!visible(Paper::RuledNarrow));
        assert!(!visible(Paper::SquaredSmall));
        // The margin is never culled, so a margin paper still shows one line.
        assert!(visible(Paper::RuledMarginNarrow));
    }
}
