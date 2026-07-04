use futures_util::{
    future::{AbortHandle, Abortable},
    SinkExt, StreamExt,
};
use gloo_net::http::Request;
use gloo_net::websocket::{futures::WebSocket, Message};
use gloo_timers::future::TimeoutFuture;
use js_sys::Reflect;
use leptos::prelude::*;
use protocol::{
    CreatePageRequest, LibraryEvent, ListPagesResponse, MeResponse, MetaResponse, PageResponse,
    PageServerMessage, PageSummary, RealtimeTicketResponse, Stroke, StrokeBatch,
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

            let me_result = Request::get("/api/me").send().await;
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

    view! {
        <main style="font-family: system-ui, sans-serif; padding: 2rem; max-width: 40rem;">
            <header style="display: flex; justify-content: space-between; align-items: center; gap: 1rem;">
                <h1 style="margin: 0;">"v-note"</h1>
                {move || match me.get() {
                    None => view! { <span>"Checking session…"</span> }.into_any(),
                    Some(Some(Ok(profile))) => view! {
                        <div style="display: flex; gap: 1rem; align-items: center;">
                            <span>{profile.email.clone().unwrap_or_else(|| profile.sub.clone())}</span>
                            <a href="/auth/logout">"Sign out"</a>
                        </div>
                    }
                    .into_any(),
                    Some(Some(Err(401))) | Some(Some(Err(403))) => view! {
                        <a href="/auth/login">"Sign in"</a>
                    }
                    .into_any(),
                    Some(Some(Err(_))) => view! {
                        <span>"Session check failed"</span>
                    }
                    .into_any(),
                    Some(None) => view! { <span>"Session unavailable"</span> }.into_any(),
                }}
            </header>

            {move || match me.get() {
                None => view! { <p>"Loading…"</p> }.into_any(),
                Some(Some(Ok(_))) => view! {
                    <section aria-label="Page library">
                        <div style="display: flex; justify-content: space-between; align-items: center; gap: 1rem;">
                            <h2>"Page library"</h2>
                            <button on:click=move |_| {
                                wasm_bindgen_futures::spawn_local(async move {
                                    if let Err(error) = create_page(pages, selected_page, library_error).await {
                                        library_error.set(Some(error));
                                    }
                                });
                            }>
                                "New page"
                            </button>
                        </div>

                        {move || library_error.get().map(|error| view! {
                            <p role="alert">{error}</p>
                        })}

                        <ul>
                            <For
                                each=move || pages.get()
                                key=|page| page.id.clone()
                                children=move |page| {
                                    let open_page = page.clone();
                                    let delete_page_id = page.id.clone();
                                    let delete_page_title = page.title.clone();
                                    view! {
                                        <li style="display: flex; gap: 0.75rem; align-items: center; margin: 0.5rem 0;">
                                            <button on:click=move |_| selected_page.set(Some(open_page.clone()))>
                                                {page.title.clone()}
                                            </button>
                                            <small>{format!("updated {}", page.updated_at)}</small>
                                            <button aria-label=format!("Delete {}", delete_page_title) on:click=move |_| {
                                                let page_id = delete_page_id.clone();
                                                wasm_bindgen_futures::spawn_local(async move {
                                                    if let Err(error) = delete_page(page_id, pages, selected_page, library_error).await {
                                                        library_error.set(Some(error));
                                                    }
                                                });
                                            }>
                                                "Delete"
                                            </button>
                                        </li>
                                    }
                                }
                            />
                        </ul>

                        {move || match selected_page.get() {
                            Some(page) => view! { <InkViewer page=page /> }.into_any(),
                            None => view! {
                                <p>"Open a page to view its canvas."</p>
                            }.into_any(),
                        }}

                        {move || match meta.get() {
                            None => view! { <p>"Loading metadata…"</p> }.into_any(),
                            Some(Ok(info)) => view! {
                                <p>{format!("Version {}", info.app_version)}</p>
                                <p>{format!("Protocol {}", info.protocol_version)}</p>
                            }
                            .into_any(),
                            Some(Err(error)) => view! {
                                <p>{format!("Failed to load metadata: {error}")}</p>
                            }
                            .into_any(),
                        }}
                    </section>
                }
                .into_any(),
                Some(Some(Err(401))) | Some(Some(Err(403))) => view! {
                    <section>
                        <p>"Sign in with Authentik to use v-note."</p>
                        <p><a href="/auth/login">"Continue to sign in"</a></p>
                    </section>
                }
                .into_any(),
                _ => view! {
                    <p>"Unable to determine authentication state."</p>
                }
                .into_any(),
            }}

            <footer style="margin-top: 2rem; font-size: 0.875rem;">
                <a href="/dl/apk">"Download Android app (.apk)"</a>
            </footer>
        </main>

        // Version watermark — compile-time build version, rendered on every screen
        // (including the pre-login front screen) with no dependency on /api/meta.
        // CI injects the computed release tag via V_NOTE_RELEASE so this matches the
        // deployed image + /api/meta; local builds fall back to the cargo version.
        <div style="position: fixed; bottom: 0.5rem; right: 0.75rem; opacity: 0.35; \
            font-size: 0.75rem; pointer-events: none; user-select: none; \
            font-family: system-ui, sans-serif;">
            {format!("v{}", release_version())}
        </div>
    }
}

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}

#[component]
fn InkViewer(page: PageSummary) -> impl IntoView {
    let canvas = NodeRef::<leptos::html::Canvas>::new();
    let batches = RwSignal::new(Vec::<StrokeBatch>::new());
    let viewer_error = RwSignal::new(None::<String>);
    let viewer_status = RwSignal::new("Connecting".to_string());
    let last_seq = RwSignal::new(0_u64);
    let offset_x = RwSignal::new(80.0_f64);
    let offset_y = RwSignal::new(80.0_f64);
    let scale = RwSignal::new(1.0_f64);
    let dragging = RwSignal::new(None::<(i32, f64, f64)>);
    let page_id = page.id.clone();
    let page_title = page.title.clone();

    Effect::new(move |_| {
        batches.set(Vec::new());
        viewer_error.set(None);
        viewer_status.set("Connecting".to_string());
        last_seq.set(0);
        let page_id = page_id.clone();
        let (abort_handle, abort_registration) = AbortHandle::new_pair();
        on_cleanup(move || abort_handle.abort());
        wasm_bindgen_futures::spawn_local(async move {
            if let Ok(Err(error)) = Abortable::new(
                page_realtime_loop(page_id, batches, viewer_status, viewer_error, last_seq),
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
        offset_x.track();
        offset_y.track();
        scale.track();
        if let Some(canvas) = canvas.get() {
            draw_canvas(
                &canvas,
                &batches.get_untracked(),
                offset_x.get_untracked(),
                offset_y.get_untracked(),
                scale.get_untracked(),
            );
        }
    });

    view! {
        <section aria-label="Open page" style="margin-top: 1.5rem;">
            <div style="display: flex; justify-content: space-between; align-items: baseline; gap: 1rem;">
                <h3>{page_title}</h3>
                <small aria-live="polite">
                    {move || format!("{} · seq {}", viewer_status.get(), last_seq.get())}
                </small>
            </div>

            {move || viewer_error.get().map(|error| view! {
                <p role="alert">{error}</p>
            })}

            <canvas
                node_ref=canvas
                width="900"
                height="520"
                aria-label="Read-only ink canvas"
                data-testid="ink-canvas"
                style="width: 100%; height: min(58vh, 520px); border: 1px solid #c9c9c9; background: #fff; touch-action: none; display: block;"
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
                    let factor = if event.delta_y() < 0.0 { 1.1 } else { 0.9 };
                    scale.update(|value| *value = (*value * factor).clamp(0.25, 4.0));
                }
            />
        </section>
    }
}

async fn page_realtime_loop(
    page_id: String,
    batches: RwSignal<Vec<StrokeBatch>>,
    viewer_status: RwSignal<String>,
    viewer_error: RwSignal<Option<String>>,
    last_seq: RwSignal<u64>,
) -> Result<(), String> {
    loop {
        match page_realtime_once(&page_id, batches, viewer_status, viewer_error, last_seq).await {
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
) -> Result<(), String> {
    let ticket_response = Request::post("/api/realtime-ticket")
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
            apply_page_event(event, batches, viewer_status, viewer_error, last_seq);
        }
    }
    Err("page realtime socket closed".to_string())
}

fn apply_page_event(
    event: PageServerMessage,
    batches: RwSignal<Vec<StrokeBatch>>,
    viewer_status: RwSignal<String>,
    viewer_error: RwSignal<Option<String>>,
    last_seq: RwSignal<u64>,
) {
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
        PageServerMessage::Error { message, .. } => {
            viewer_error.set(Some(message));
        }
        PageServerMessage::LeaseGranted
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

fn draw_canvas(
    canvas: &HtmlCanvasElement,
    batches: &[StrokeBatch],
    offset_x: f64,
    offset_y: f64,
    scale: f64,
) {
    let Ok(Some(context)) = canvas.get_context("2d") else {
        return;
    };
    let Ok(context) = context.dyn_into::<CanvasRenderingContext2d>() else {
        return;
    };
    let width = canvas.width() as f64;
    let height = canvas.height() as f64;
    context.set_fill_style_str("#ffffff");
    context.fill_rect(0.0, 0.0, width, height);
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

fn draw_stroke(
    context: &CanvasRenderingContext2d,
    stroke: &Stroke,
    offset_x: f64,
    offset_y: f64,
    scale: f64,
) {
    context.begin_path();
    context.set_stroke_style_str(&stroke.color);
    context.set_line_width(2.0);
    if let Some(first) = stroke.points.first() {
        context.move_to(first.x * scale + offset_x, first.y * scale + offset_y);
        for point in stroke.points.iter().skip(1) {
            context.line_to(point.x * scale + offset_x, point.y * scale + offset_y);
        }
    }
    context.stroke();
}

async fn load_pages(
    pages: RwSignal<Vec<PageSummary>>,
    library_error: RwSignal<Option<String>>,
) -> Result<(), String> {
    let response = Request::get("/api/pages")
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

async fn create_page(
    pages: RwSignal<Vec<PageSummary>>,
    selected_page: RwSignal<Option<PageSummary>>,
    library_error: RwSignal<Option<String>>,
) -> Result<(), String> {
    let response = Request::post("/api/pages")
        .json(&CreatePageRequest {
            title: Some("Untitled page".to_string()),
        })
        .map_err(|error| format!("create request failed: {error}"))?
        .send()
        .await
        .map_err(|error| format!("creating page failed: {error}"))?;
    if !response.ok() {
        return Err(format!("creating page failed: HTTP {}", response.status()));
    }
    let body = response
        .json::<PageResponse>()
        .await
        .map_err(|error| format!("invalid create response: {error}"))?;
    upsert_page(pages, body.page.clone());
    selected_page.set(Some(body.page));
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
    match event {
        LibraryEvent::PageCreated { page } => upsert_page(pages, page),
        LibraryEvent::PageDeleted { page_id } => remove_page(pages, selected_page, &page_id),
    }
}

fn upsert_page(pages: RwSignal<Vec<PageSummary>>, page: PageSummary) {
    pages.update(|items| {
        items.retain(|existing| existing.id != page.id);
        items.push(page);
        items.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
    });
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
