//! `message_type()` is the metric label for every realtime frame. Its match is
//! exhaustive, so a new variant cannot compile without a label; these tests hold
//! each label to the `type` tag serde actually writes, so the label on a
//! dashboard is always the string a client sees on the wire.
//!
//! Each enum has a `*_index` function, also an exhaustive match, and a variant
//! count beside it. Adding a variant breaks compilation there as well, and the
//! coverage assertion then fails until a sample for the new variant is added.

use std::collections::BTreeSet;

use protocol::{
    LibraryEvent, PageClientMessage, PageServerMessage, PageSummary, Paper, StrokeBatch,
    ThumbnailMetadata, TombstoneBatch,
};
use serde::Serialize;

fn serde_tag(message: &impl Serialize) -> String {
    serde_json::to_value(message).expect("message should serialize")["type"]
        .as_str()
        .expect("message should carry a string `type` tag")
        .to_string()
}

/// Asserts the samples cover `0..variant_count` exactly once each, that every
/// label equals the serde tag, and that no two variants share a label.
fn assert_labels_match_serde<T: Serialize>(
    samples: &[T],
    variant_count: usize,
    index: impl Fn(&T) -> usize,
    label: impl Fn(&T) -> &'static str,
) {
    let indices: Vec<usize> = samples.iter().map(&index).collect();
    assert_eq!(
        indices.iter().copied().collect::<BTreeSet<_>>(),
        (0..variant_count).collect::<BTreeSet<_>>(),
        "every variant needs exactly one sample"
    );
    assert_eq!(indices.len(), variant_count, "one sample per variant");

    let mut labels = BTreeSet::new();
    for sample in samples {
        assert_eq!(label(sample), serde_tag(sample));
        assert!(labels.insert(label(sample)), "labels must be distinct");
    }
}

const PAGE_SERVER_VARIANTS: usize = 9;

fn page_server_index(message: &PageServerMessage) -> usize {
    match message {
        PageServerMessage::Welcome { .. } => 0,
        PageServerMessage::StrokeBatch(_) => 1,
        PageServerMessage::TombstoneBatch(_) => 2,
        PageServerMessage::Synced { .. } => 3,
        PageServerMessage::LeaseGranted => 4,
        PageServerMessage::LeaseDenied { .. } => 5,
        PageServerMessage::PaperChanged { .. } => 6,
        PageServerMessage::LeaseChanged { .. } => 7,
        PageServerMessage::Error { .. } => 8,
    }
}

#[test]
fn page_server_message_types_match_serde_tags() {
    let samples = [
        PageServerMessage::Welcome {
            session_id: "session_1".to_string(),
            last_seq: 0,
            lease_holder: None,
            paper: Paper::default(),
        },
        PageServerMessage::StrokeBatch(StrokeBatch {
            seq: 1,
            client_batch_id: "batch_1".to_string(),
            strokes: Vec::new(),
        }),
        PageServerMessage::TombstoneBatch(TombstoneBatch {
            revision: 2,
            client_mutation_id: "mutation_1".to_string(),
            stroke_ids: Vec::new(),
        }),
        PageServerMessage::Synced { last_seq: 1 },
        PageServerMessage::LeaseGranted,
        PageServerMessage::LeaseDenied {
            holder: "session_2".to_string(),
        },
        PageServerMessage::PaperChanged {
            paper: Paper::default(),
            revision: 3,
        },
        PageServerMessage::LeaseChanged { holder: None },
        PageServerMessage::Error {
            code: "bad_message".to_string(),
            message: "could not parse client message".to_string(),
            client_mutation_id: None,
        },
    ];
    assert_labels_match_serde(
        &samples,
        PAGE_SERVER_VARIANTS,
        page_server_index,
        PageServerMessage::message_type,
    );
}

const PAGE_CLIENT_VARIANTS: usize = 7;

fn page_client_index(message: &PageClientMessage) -> usize {
    match message {
        PageClientMessage::Subscribe { .. } => 0,
        PageClientMessage::AcquireLease => 1,
        PageClientMessage::RenewLease => 2,
        PageClientMessage::ReleaseLease => 3,
        PageClientMessage::CommitBatch { .. } => 4,
        PageClientMessage::CommitTombstones { .. } => 5,
        PageClientMessage::SetPaper { .. } => 6,
    }
}

#[test]
fn page_client_message_types_match_serde_tags() {
    let samples = [
        PageClientMessage::Subscribe { from_seq: 0 },
        PageClientMessage::AcquireLease,
        PageClientMessage::RenewLease,
        PageClientMessage::ReleaseLease,
        PageClientMessage::CommitBatch {
            client_batch_id: "batch_1".to_string(),
            strokes: Vec::new(),
        },
        PageClientMessage::CommitTombstones {
            client_mutation_id: "mutation_1".to_string(),
            stroke_ids: Vec::new(),
        },
        PageClientMessage::SetPaper {
            client_mutation_id: "mutation_2".to_string(),
            paper: Paper::default(),
        },
    ];
    assert_labels_match_serde(
        &samples,
        PAGE_CLIENT_VARIANTS,
        page_client_index,
        PageClientMessage::message_type,
    );
}

const LIBRARY_EVENT_VARIANTS: usize = 4;

fn library_event_index(event: &LibraryEvent) -> usize {
    match event {
        LibraryEvent::PageCreated { .. } => 0,
        LibraryEvent::PageDeleted { .. } => 1,
        LibraryEvent::PageThumbnailUpdated { .. } => 2,
        LibraryEvent::PageUpdated { .. } => 3,
    }
}

#[test]
fn library_event_types_match_serde_tags() {
    let samples = [
        LibraryEvent::PageCreated {
            page: PageSummary {
                id: "page_1".to_string(),
                title: String::new(),
                created_at: "2026-09-14T00:00:00Z".to_string(),
                updated_at: "2026-09-14T00:00:00Z".to_string(),
                thumbnail: ThumbnailMetadata::Empty,
                paper: Paper::default(),
            },
        },
        LibraryEvent::PageDeleted {
            page_id: "page_1".to_string(),
        },
        LibraryEvent::PageThumbnailUpdated {
            page_id: "page_1".to_string(),
            thumbnail: ThumbnailMetadata::Generating { source_seq: 1 },
        },
        LibraryEvent::PageUpdated {
            page_id: "page_1".to_string(),
            updated_at: "2026-09-14T00:00:00Z".to_string(),
        },
    ];
    assert_labels_match_serde(
        &samples,
        LIBRARY_EVENT_VARIANTS,
        library_event_index,
        LibraryEvent::message_type,
    );
}
