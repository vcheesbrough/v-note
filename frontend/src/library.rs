//! The page library's state: display titles, the recent-first page list and
//! the reducers that apply library events to it. Pure over the vector where it
//! can be, so the ordering rules are unit-tested without a reactive runtime.

use js_sys::Date;
use leptos::prelude::*;
use protocol::{LibraryEvent, PageSummary, ThumbnailMetadata};

const UNTITLED_PAGE: &str = "Untitled page";

pub(crate) fn page_has_title(page: &PageSummary) -> bool {
    let title = page.title.trim();
    !title.is_empty() && title != UNTITLED_PAGE
}

pub(crate) fn page_display_title(page: &PageSummary) -> String {
    if page_has_title(page) {
        page.title.clone()
    } else {
        approximate_relative_datetime(&page.updated_at)
    }
}

fn compact_datetime(value: &str) -> String {
    value
        .trim_end_matches('Z')
        .replace('T', " ")
        .chars()
        .take(16)
        .collect()
}

pub(crate) fn approximate_relative_datetime(value: &str) -> String {
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

pub(crate) fn apply_event(
    pages: RwSignal<Vec<PageSummary>>,
    selected_page: RwSignal<Option<PageSummary>>,
    event: LibraryEvent,
) {
    // Closing a deleted page is the only cross-signal effect; the list mutation
    // itself is a pure reducer so it can be unit-tested.
    if let LibraryEvent::PageDeleted { page_id } = &event {
        close_if_open(selected_page, page_id);
    }
    pages.update(|items| apply_library_event_to_pages(items, event));
}

/// Close the viewer if it is showing `page_id`, and take that id out of the
/// address bar with it: the page is gone, so a reload or a Back into that entry
/// must not try to open it again. Rewriting rather than pushing keeps the
/// deleted page out of Forward too.
fn close_if_open(selected_page: RwSignal<Option<PageSummary>>, page_id: &str) {
    if selected_page
        .get_untracked()
        .as_ref()
        .is_some_and(|page| page.id == page_id)
    {
        selected_page.set(None);
        crate::route::replace(&crate::route::Route::Library);
    }
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
            if let Some(page) = items.iter_mut().find(|page| page.id == page_id)
                && thumbnail_seq(&thumbnail) >= thumbnail_seq(&page.thumbnail)
            {
                page.thumbnail = thumbnail;
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

pub(crate) fn thumbnail_key(thumbnail: &ThumbnailMetadata) -> String {
    match thumbnail {
        ThumbnailMetadata::Empty => "empty".to_string(),
        ThumbnailMetadata::Generating { source_seq } => format!("generating:{source_seq}"),
        ThumbnailMetadata::Available { source_seq, url } => format!("available:{source_seq}:{url}"),
        ThumbnailMetadata::Failed { source_seq } => format!("failed:{source_seq}"),
    }
}

pub(crate) fn remove_page(
    pages: RwSignal<Vec<PageSummary>>,
    selected_page: RwSignal<Option<PageSummary>>,
    page_id: &str,
) {
    pages.update(|items| items.retain(|page| page.id != page_id));
    close_if_open(selected_page, page_id);
}

#[cfg(test)]
mod tests {
    use protocol::Paper;

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
}
