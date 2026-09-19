//! v-note SPA: a read-only viewer over the page library. `main` mounts `App`,
//! which owns the session state and switches between the library and the
//! `viewer::InkViewer` for the selected page. Both screens wear the same
//! `.top-bar`, so the chrome does not change shape when a page opens.
//!
//! Which screen is showing is held in the address bar (`route`), not only in a
//! signal, so pages are linkable and the browser's Back button walks the app's
//! own history instead of leaving it.

mod api;
mod library;
mod realtime;
mod render;
mod route;
mod viewer;

use leptos::prelude::*;
use leptos::{ev, html, leptos_dom::helpers::window_event_listener};
use protocol::{MeResponse, MetaResponse, PageSummary, ThumbnailMetadata};
use wasm_bindgen::JsCast;

use crate::library::{approximate_relative_datetime, page_has_title, remove_page, thumbnail_key};
use crate::route::{Leave, Route};
use crate::viewer::InkViewer;

/// What `/api/me` has told us so far: `None` while the request is in flight,
/// `Err(401 | 403)` for a signed-out browser, any other `Err` for a failure we
/// cannot interpret as a session state.
type Session = Option<Result<MeResponse, u16>>;

fn signed_out(status: u16) -> bool {
    status == 401 || status == 403
}

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

/// The library's hamburger menu: identity, the Android download and sign out.
/// `/dl/apk` is public, so the download is offered without a session too;
/// signing in belongs to the bar, not here.
///
/// `<details>` gives the disclosure for free but nothing that closes it again —
/// the browser only ever toggles it from the summary. So clicking away, Escape
/// and picking an item all have to clear `open` by hand, and the apk item makes
/// that visible: it downloads without navigating, so nothing else would.
#[component]
fn MainMenu(me: RwSignal<Session>) -> impl IntoView {
    let menu: NodeRef<html::Details> = NodeRef::new();
    let summary: NodeRef<html::Summary> = NodeRef::new();
    let close = move || {
        if let Some(menu) = menu.get_untracked() {
            menu.set_open(false);
        }
    };

    Effect::new(move |_| {
        // Only a click that lands outside the disclosure dismisses it. One
        // inside must not: the summary's own click bubbles to here too, and
        // closing on it would undo the toggle that just opened the menu.
        let click = window_event_listener(ev::click, move |event| {
            let Some(menu) = menu.get_untracked() else {
                return;
            };
            let inside = event
                .target()
                .and_then(|target| target.dyn_into::<web_sys::Node>().ok())
                .is_some_and(|node| menu.contains(Some(&node)));
            if !inside {
                close();
            }
        });
        on_cleanup(move || click.remove());
    });

    Effect::new(move |_| {
        let keydown = window_event_listener(ev::keydown, move |event| {
            if event.key() != "Escape" {
                return;
            }
            let Some(menu) = menu.get_untracked() else {
                return;
            };
            // Only an open menu answers Escape, or every Escape in the library
            // would pull focus to the hamburger.
            if !menu.open() {
                return;
            }
            menu.set_open(false);
            // A keyboard dismissal hands focus back to the control that owns
            // the panel instead of letting it fall to `body`, which would send
            // the next Tab back to the top of the document. Click-away must not
            // do this: there the focus belongs wherever the user clicked.
            if let Some(summary) = summary.get_untracked() {
                let _ = summary.focus();
            }
        });
        on_cleanup(move || keydown.remove());
    });

    view! {
        <details class="app-menu" node_ref=menu>
            <summary node_ref=summary aria-label="Open main menu"><span aria-hidden="true">"☰"</span></summary>
            <div class="app-menu-panel">
                {move || match me.get() {
                    Some(Ok(profile)) => view! {
                        <p class="menu-identity">{profile.email.clone().unwrap_or_else(|| profile.sub.clone())}</p>
                        <a class="menu-link" href=apk_download_url() download=apk_download_filename() on:click=move |_| close()>"Download Android app (.apk)"</a>
                        <a class="menu-link" href="/auth/logout" on:click=move |_| close()>"Sign out"</a>
                    }
                    .into_any(),
                    // Without a session there is nothing to say about the
                    // account, and signing in is the bar's control — the menu
                    // must not offer one for a state it has not established.
                    _ => view! {
                        <a class="menu-link" href=apk_download_url() download=apk_download_filename() on:click=move |_| close()>"Download Android app (.apk)"</a>
                    }
                    .into_any(),
                }}
            </div>
        </details>
    }
}

/// The library's top bar: menu and brand on the left, the session control on
/// the right. Signed out it is a sign-in button, so the library is reachable
/// (empty) without an account.
#[component]
fn LibraryBar(me: RwSignal<Session>) -> impl IntoView {
    view! {
        <header class="top-bar">
            <div class="bar-left">
                <MainMenu me=me />
                <h1 class="brand">"v-note"</h1>
            </div>
            <div class="bar-right">
                {move || match me.get() {
                    None => view! { <span class="muted">"Checking session…"</span> }.into_any(),
                    Some(Ok(_)) => ().into_any(),
                    Some(Err(status)) if signed_out(status) => view! {
                        // A full-page hand-off to the IdP, which always returns
                        // the browser to `/` — so stash the deep link first.
                        <a class="button primary" href="/auth/login" on:click=move |_| route::remember_return_route()>"Sign in"</a>
                    }
                    .into_any(),
                    Some(Err(_)) => view! { <span class="muted">"Session check failed"</span> }.into_any(),
                }}
            </div>
        </header>
    }
}

/// One page tile: the thumbnail that opens the page, its age, and delete.
#[component]
fn PageTile(
    page: PageSummary,
    pages: RwSignal<Vec<PageSummary>>,
    selected_page: RwSignal<Option<PageSummary>>,
    library_error: RwSignal<Option<String>>,
) -> impl IntoView {
    let preview_page = page.clone();
    let delete_page_id = page.id.clone();
    let display_title = page_has_title(&page).then(|| page.title.clone());
    let page_age = approximate_relative_datetime(&page.updated_at);
    let open_label = display_title
        .as_ref()
        .map_or_else(|| "Open page".to_string(), |title| format!("Open {title}"));
    let delete_label = display_title.as_ref().map_or_else(
        || "Delete page".to_string(),
        |title| format!("Delete {title}"),
    );
    let thumbnail = page.thumbnail.clone();
    let thumbnail_unavailable = library_error.get_untracked().is_some();

    view! {
        <li class="page-tile">
            <button class="page-preview-button" aria-label=open_label on:click=move |_| {
                let page = preview_page.clone();
                // Push before selecting: the entry the user goes Back to is the
                // library they are standing on.
                route::push(&Route::Page(page.id.clone()));
                selected_page.set(Some(page));
            }>
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
            {display_title.map(|title| view! {
                <span class="page-thumbnail-title">{title}</span>
            })}
            </button>
            <div class="page-tile-footer">
                <span class="page-age">{page_age}</span>
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

/// The library body under the top bar: the error banner, the page grid (or the
/// empty state that stands in for it), and the build footer. Rendered signed
/// out too, with no pages — signing in fills it rather than replacing it.
#[component]
fn Library(
    me: RwSignal<Session>,
    meta: RwSignal<Option<Result<MetaResponse, String>>>,
    pages: RwSignal<Vec<PageSummary>>,
    selected_page: RwSignal<Option<PageSummary>>,
    library_error: RwSignal<Option<String>>,
    route_notice: RwSignal<Option<String>>,
) -> impl IntoView {
    view! {
        <section class="library-shell" aria-label="Page library">
            // Why the address bar did not open what it named. Its own signal
            // because it outlives a reconnect: `library_error` is the live
            // transport's state and every successful reload clears it.
            {move || route_notice.get().map(|notice| view! {
                <p class="alert" role="alert">{notice}</p>
            })}

            {move || library_error.get().map(|error| view! {
                <p class="alert" role="alert">{error}</p>
            })}

            {move || match me.get() {
                None => view! {
                    <div class="state-panel">
                        <p class="eyebrow">"Session"</p>
                        <h2 class="section-title">"Checking access"</h2>
                        <p class="muted">"Loading your private v-note session."</p>
                    </div>
                }.into_any(),
                // The sign-in control lives in the top bar; this is the empty
                // library it leaves behind, not a second call to action.
                Some(Err(status)) if signed_out(status) => view! {
                    <div class="state-panel">
                        <p class="eyebrow">"Private notes"</p>
                        <h2 class="section-title">"Sign in to see your pages"</h2>
                        <p class="muted">"Your pages are private to your account."</p>
                    </div>
                }.into_any(),
                Some(Err(_)) => view! {
                    <div class="state-panel" role="alert">
                        <p class="eyebrow">"Session"</p>
                        <h2 class="section-title">"Unable to determine authentication state"</h2>
                    </div>
                }.into_any(),
                Some(Ok(_)) if pages.get().is_empty() => view! {
                    <div class="state-panel">
                        <p class="eyebrow">"No pages"</p>
                        <h2 class="section-title">"Start with a blank ink page"</h2>
                        <p class="muted">"Create a page on Android, then open it here to follow the ink live."</p>
                    </div>
                }.into_any(),
                Some(Ok(_)) => view! {
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
                            children=move |page| view! {
                                <PageTile page=page pages=pages selected_page=selected_page library_error=library_error />
                            }
                        />
                    </ul>
                }.into_any(),
            }}

            {move || match meta.get() {
                None => view! { <p class="muted build-line">"Loading metadata…"</p> }.into_any(),
                Some(Ok(info)) => view! {
                    <p class="muted build-line">{format!("Version {} · Protocol {}", info.app_version, info.protocol_version)}</p>
                }
                .into_any(),
                Some(Err(error)) => view! {
                    <p class="alert" role="alert">{format!("Failed to load metadata: {error}")}</p>
                }
                .into_any(),
            }}
        </section>
    }
}

#[component]
fn App() -> impl IntoView {
    let meta = RwSignal::new(None::<Result<MetaResponse, String>>);
    let me = RwSignal::new(None::<Result<MeResponse, u16>>);
    let pages = RwSignal::new(Vec::<PageSummary>::new());
    let selected_page = RwSignal::new(None::<PageSummary>);
    let library_error = RwSignal::new(None::<String>);
    // Why a route did not open what it named — a deep link to a page this
    // owner does not have.
    let route_notice = RwSignal::new(None::<String>);
    // A page id the URL names that the library has not delivered yet. A cold
    // load of `/p/{id}` knows the id long before it knows the page.
    let pending_page_id = RwSignal::new(None::<String>);
    // Whether `/api/pages` has answered at all; set by `api::load_pages`, so
    // the retrying library socket establishes it too.
    let pages_loaded = RwSignal::new(false);

    // The address bar is read before anything else, so a deep link opens its
    // page rather than flashing the library on the way there.
    if let Route::Page(page_id) = route::initial_route() {
        pending_page_id.set(Some(page_id));
    }

    // Show whichever page the URL names, or park the id until the library
    // arrives with it.
    let show_page_id = move |page_id: String| match pages
        .get_untracked()
        .iter()
        .find(|page| page.id == page_id)
        .cloned()
    {
        Some(page) => {
            pending_page_id.set(None);
            selected_page.set(Some(page));
        }
        None => {
            selected_page.set(None);
            pending_page_id.set(Some(page_id));
        }
    };

    Effect::new(move |_| {
        wasm_bindgen_futures::spawn_local(async move {
            meta.set(Some(api::fetch_meta().await));
            me.set(Some(api::fetch_me().await));
        });
    });

    Effect::new(move |_| {
        if matches!(me.get(), Some(Ok(_))) {
            wasm_bindgen_futures::spawn_local(async move {
                if let Err(error) = api::load_pages(pages, pages_loaded, library_error).await {
                    library_error.set(Some(error));
                }
            });
            wasm_bindgen_futures::spawn_local(realtime::library_realtime_loop(
                pages,
                selected_page,
                pages_loaded,
                library_error,
            ));
        }
    });

    // Resolve a deep-linked id against the library as soon as the library can
    // answer. Only once it has answered is "not here" a real answer: before
    // that the list is empty because nothing has loaded, and while signed out
    // the URL is left alone so signing in still lands on the linked page.
    Effect::new(move |_| {
        let Some(page_id) = pending_page_id.get() else {
            return;
        };
        match pages.get().iter().find(|page| page.id == page_id).cloned() {
            Some(page) => {
                pending_page_id.set(None);
                selected_page.set(Some(page));
            }
            None if pages_loaded.get() => {
                pending_page_id.set(None);
                route::replace(&Route::Library);
                route_notice.set(Some("That page is not in your library.".to_string()));
            }
            None => {}
        }
    });

    // Back and Forward are the same navigation as a tile click or the back
    // arrow, so they run through the same code: the URL says what to show.
    Effect::new(move |_| {
        let popstate = window_event_listener(ev::popstate, move |_| {
            route::note_popstate();
            match route::current_route() {
                Route::Library => {
                    pending_page_id.set(None);
                    selected_page.set(None);
                }
                Route::Page(page_id) => show_page_id(page_id),
            }
        });
        on_cleanup(move || popstate.remove());
    });

    // Opening anything answers the notice: it is about a link that failed, not
    // about the library itself.
    Effect::new(move |_| {
        if selected_page.get().is_some() {
            route_notice.set(None);
        }
    });

    let viewer_open = move || selected_page.get().is_some();

    view! {
        <main class=move || if viewer_open() { "app-shell viewer-mode" } else { "app-shell" }>
            {move || match selected_page.get() {
                // The viewer brings its own `.top-bar` — same bar, different
                // contents — so exactly one is on screen either way.
                Some(page) => view! {
                    // Leaving pops the entry the tile pushed where there is one;
                    // only a rewritten URL (a cold deep link) leaves the screen
                    // change to us, since no `popstate` follows it.
                    <InkViewer page=page on_close=Callback::new(move |_| {
                        if route::leave_page() == Leave::Rewrote {
                            selected_page.set(None);
                        }
                    }) />
                }.into_any(),
                None => view! {
                    <LibraryBar me=me />
                    <Library me=me meta=meta pages=pages selected_page=selected_page library_error=library_error route_notice=route_notice />
                }.into_any(),
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
