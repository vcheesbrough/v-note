//! v-note SPA: a read-only viewer over the page library. `main` mounts `App`,
//! which owns the session state and switches between the library and the
//! `viewer::InkViewer` for the selected page.

mod api;
mod library;
mod realtime;
mod render;
mod viewer;

use leptos::prelude::*;
use protocol::{MeResponse, MetaResponse, PageSummary, ThumbnailMetadata};

use crate::library::{approximate_relative_datetime, page_has_title, remove_page, thumbnail_key};
use crate::viewer::InkViewer;

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

#[component]
fn App() -> impl IntoView {
    let meta = RwSignal::new(None::<Result<MetaResponse, String>>);
    let me = RwSignal::new(None::<Option<Result<MeResponse, u16>>>);
    let pages = RwSignal::new(Vec::<PageSummary>::new());
    let selected_page = RwSignal::new(None::<PageSummary>);
    let library_error = RwSignal::new(None::<String>);

    Effect::new(move |_| {
        wasm_bindgen_futures::spawn_local(async move {
            meta.set(Some(api::fetch_meta().await));
            me.set(Some(Some(api::fetch_me().await)));
        });
    });

    Effect::new(move |_| {
        if matches!(me.get(), Some(Some(Ok(_)))) {
            wasm_bindgen_futures::spawn_local(async move {
                if let Err(error) = api::load_pages(pages, library_error).await {
                    library_error.set(Some(error));
                }
            });
            wasm_bindgen_futures::spawn_local(realtime::library_realtime_loop(
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
                // `view! {}` expands to `()`; spell the unit out so the empty
                // branch does not trip `unused_unit`/`unit_arg` inside the macro.
                ().into_any()
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
                                                        match api::delete_page(&page_id).await {
                                                            Ok(()) => {
                                                                remove_page(pages, selected_page, &page_id);
                                                                library_error.set(None);
                                                            }
                                                            Err(error) => library_error.set(Some(error)),
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
    mount_to_body(App);
}
