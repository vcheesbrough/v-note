//! The SPA's navigation state, which is the address bar. `/` is the library and
//! `/p/{page_id}` the open page, so a page can be linked, bookmarked and
//! reloaded — and, because opening one pushes a history entry, the browser's
//! Back button has something of ours to pop instead of leaving v-note (#189).
//!
//! `route_from_path` / `path_for_route` are pure so the scheme is unit-tested
//! without a browser; everything below them touches `window.history`.

use std::cell::Cell;

use wasm_bindgen::JsValue;

/// Path prefix for the viewer. Short, and it cannot collide with `/api`,
/// `/auth`, `/health`, `/dl` or the hashed assets Trunk emits at the root.
const PAGE_PREFIX: &str = "/p/";

/// Where a signed-out deep link parks the path it was aiming at. `/auth/callback`
/// always returns the browser to `/`, so without this the sign-in round trip
/// would quietly swallow the link the user actually followed.
const RETURN_TO_KEY: &str = "v-note:return-to";

thread_local! {
    /// Whether the app has moved within its own history yet. A cold load
    /// straight into `/p/{id}` starts `false`: that entry is the document's
    /// first, so `history.back()` would leave v-note rather than reveal the
    /// library behind it.
    static NAVIGATED: Cell<bool> = const { Cell::new(false) };
}

/// The two screens the SPA has.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Route {
    Library,
    Page(String),
}

/// What `leave_page` did, because the two cases finish differently: a pop is
/// answered by the `popstate` handler, a rewrite by the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Leave {
    /// A `popstate` is on its way and will close the viewer.
    Popping,
    /// The URL was rewritten in place; the caller clears the selection.
    Rewrote,
}

/// Read a route out of a path. Anything unrecognised is the library rather than
/// an error screen — an old or hand-edited link should land somewhere useful.
pub(crate) fn route_from_path(path: &str) -> Route {
    let path = path
        .split(['?', '#'])
        .next()
        .unwrap_or(path)
        .trim_end_matches('/');
    match path.strip_prefix(PAGE_PREFIX.trim_end_matches('/')) {
        // `/p` and `/p/` name no page; `/p/a/b` is not a page id.
        Some(rest) => match rest.strip_prefix('/') {
            Some(page_id) if !page_id.is_empty() && !page_id.contains('/') => {
                Route::Page(page_id.to_string())
            }
            _ => Route::Library,
        },
        None => Route::Library,
    }
}

pub(crate) fn path_for_route(route: &Route) -> String {
    match route {
        Route::Library => "/".to_string(),
        Route::Page(page_id) => format!("{PAGE_PREFIX}{page_id}"),
    }
}

fn history() -> Option<web_sys::History> {
    web_sys::window()?.history().ok()
}

fn session_storage() -> Option<web_sys::Storage> {
    web_sys::window()?.session_storage().ok().flatten()
}

/// The route the address bar names right now.
pub(crate) fn current_route() -> Route {
    let path = web_sys::window()
        .and_then(|window| window.location().pathname().ok())
        .unwrap_or_else(|| "/".to_string());
    route_from_path(&path)
}

/// The route the app starts on: the address bar, unless a sign-in round trip
/// dropped us back on `/` with a deep link still owed. Consumes the stash, so
/// the debt is only ever paid once.
pub(crate) fn initial_route() -> Route {
    match current_route() {
        Route::Library => match take_return_route() {
            Some(route) => {
                replace(&route);
                route
            }
            None => Route::Library,
        },
        route => route,
    }
}

/// Open a route in a new history entry, so Back returns to the one before it.
pub(crate) fn push(route: &Route) {
    if let Some(history) = history() {
        let _ = history.push_state_with_url(&JsValue::NULL, "", Some(&path_for_route(route)));
        NAVIGATED.with(|navigated| navigated.set(true));
    }
}

/// Rewrite the current history entry, for a move the user should not be able to
/// go Forward into again — a resolved deep link, or a page that has been deleted
/// out from under the viewer.
pub(crate) fn replace(route: &Route) {
    if let Some(history) = history() {
        let _ = history.replace_state_with_url(&JsValue::NULL, "", Some(&path_for_route(route)));
    }
}

/// Note that the browser moved us: after a `popstate` there is app history on
/// both sides of where we stand.
pub(crate) fn note_popstate() {
    NAVIGATED.with(|navigated| navigated.set(true));
}

/// Leave the viewer from the app's own back control. Pops the entry we pushed so
/// the stack does not grow by two on every open/close; falls back to rewriting
/// the URL when the viewer *is* the first entry of the document, where `back()`
/// would take the user off v-note entirely.
pub(crate) fn leave_page() -> Leave {
    if NAVIGATED.with(Cell::get)
        && let Some(history) = history()
        && history.back().is_ok()
    {
        return Leave::Popping;
    }
    replace(&Route::Library);
    Leave::Rewrote
}

/// Remember where we were before handing the browser to the IdP. Only a page is
/// worth remembering — the library is where sign-in lands anyway.
pub(crate) fn remember_return_route() {
    let route = current_route();
    if let Route::Page(_) = route
        && let Some(storage) = session_storage()
    {
        let _ = storage.set_item(RETURN_TO_KEY, &path_for_route(&route));
    }
}

fn take_return_route() -> Option<Route> {
    let storage = session_storage()?;
    let path = storage.get_item(RETURN_TO_KEY).ok().flatten()?;
    let _ = storage.remove_item(RETURN_TO_KEY);
    match route_from_path(&path) {
        Route::Page(page_id) => Some(Route::Page(page_id)),
        Route::Library => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_is_the_library() {
        assert_eq!(route_from_path("/"), Route::Library);
        assert_eq!(route_from_path(""), Route::Library);
    }

    #[test]
    fn page_path_carries_the_id() {
        assert_eq!(
            route_from_path("/p/page_0123456789abcdef"),
            Route::Page("page_0123456789abcdef".to_string())
        );
    }

    #[test]
    fn a_trailing_slash_names_the_same_page() {
        assert_eq!(
            route_from_path("/p/page_a/"),
            Route::Page("page_a".to_string())
        );
    }

    /// A query or fragment is not part of the page id — `location.pathname` never
    /// carries one, but the stashed return path goes through here too.
    #[test]
    fn query_and_fragment_are_not_part_of_the_id() {
        assert_eq!(
            route_from_path("/p/page_a?from=mail"),
            Route::Page("page_a".to_string())
        );
        assert_eq!(
            route_from_path("/p/page_a#ink"),
            Route::Page("page_a".to_string())
        );
    }

    /// Anything that does not name exactly one page falls back to the library
    /// rather than opening a viewer on nothing.
    #[test]
    fn paths_that_name_no_page_fall_back_to_the_library() {
        for path in [
            "/p",
            "/p/",
            "/p/a/b",
            "/pages/page_a",
            "/api/pages",
            "/nope",
        ] {
            assert_eq!(route_from_path(path), Route::Library, "{path}");
        }
    }

    #[test]
    fn routes_round_trip_through_their_path() {
        for route in [Route::Library, Route::Page("page_a".to_string())] {
            assert_eq!(route_from_path(&path_for_route(&route)), route);
        }
        assert_eq!(path_for_route(&Route::Library), "/");
        assert_eq!(
            path_for_route(&Route::Page("page_a".to_string())),
            "/p/page_a"
        );
    }
}
