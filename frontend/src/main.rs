use futures_util::StreamExt;
use gloo_net::http::Request;
use gloo_net::websocket::{futures::WebSocket, Message};
use gloo_timers::future::TimeoutFuture;
use leptos::prelude::*;
use protocol::{
    CreatePageRequest, LibraryEvent, ListPagesResponse, MeResponse, MetaResponse, PageResponse,
    PageSummary, RealtimeTicketResponse,
};

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
            wasm_bindgen_futures::spawn_local(library_realtime_loop(pages, library_error));
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
                            {move || pages.get().into_iter().map(|page| {
                                let open_page = page.clone();
                                let delete_page_id = page.id.clone();
                                view! {
                                    <li style="display: flex; gap: 0.75rem; align-items: center; margin: 0.5rem 0;">
                                        <button on:click=move |_| selected_page.set(Some(open_page.clone()))>
                                            {page.title.clone()}
                                        </button>
                                        <small>{format!("updated {}", page.updated_at)}</small>
                                        <button aria-label=format!("Delete {}", page.title) on:click=move |_| {
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
                            }).collect_view()}
                        </ul>

                        {move || match selected_page.get() {
                            Some(page) => view! {
                                <section aria-label="Open page">
                                    <h3>{page.title}</h3>
                                    <p>"Empty canvas placeholder. Ink capture lands in the next iteration."</p>
                                </section>
                            }.into_any(),
                            None => view! {
                                <p>"Open a page to view its empty canvas placeholder."</p>
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
    library_error: RwSignal<Option<String>>,
) {
    loop {
        if let Err(error) = library_realtime_once(pages, library_error).await {
            library_error.set(Some(error));
            TimeoutFuture::new(1_000).await;
        }
    }
}

async fn library_realtime_once(
    pages: RwSignal<Vec<PageSummary>>,
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
            apply_event(pages, event);
            library_error.set(None);
        }
    }

    Err("realtime socket closed".to_string())
}

fn realtime_url(ticket: &str) -> Result<String, String> {
    let window = web_sys::window().ok_or_else(|| "window unavailable".to_string())?;
    let location = window.location();
    let protocol = location
        .protocol()
        .map_err(|_| "location protocol unavailable".to_string())?;
    let host = location
        .host()
        .map_err(|_| "location host unavailable".to_string())?;
    let scheme = if protocol == "https:" { "wss" } else { "ws" };
    Ok(format!("{scheme}://{host}/api/realtime?ticket={ticket}"))
}

fn apply_event(pages: RwSignal<Vec<PageSummary>>, event: LibraryEvent) {
    match event {
        LibraryEvent::PageCreated { page } => upsert_page(pages, page),
        LibraryEvent::PageDeleted { page_id } => {
            pages.update(|items| items.retain(|page| page.id != page_id));
        }
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
