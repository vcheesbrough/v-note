//! Contract schema gate (#337).
//!
//! `schemas/` describes the wire contract as JSON Schema and
//! `contracts/fixtures/` holds the golden payloads every client asserts.
//! Nothing checked one against the other until this test, so the stroke
//! schema fell two iterations behind the wire format without anyone noticing.
//!
//! Three checks, all run by `cargo test -p protocol` (CI `checks` → `rust-test`):
//!
//! 1. every schema is itself a valid JSON Schema 2020-12 document;
//! 2. every fixture validates against its schema — and every fixture is mapped
//!    to one, so a new fixture without a schema fails here;
//! 3. every variant of the tagged message enums, **serialised by serde**,
//!    validates against its schema, and the schema's `oneOf` branches cover
//!    exactly those variants. A new variant without a branch (or a stale
//!    branch) fails here before any fixture exists for it. The REST DTOs get
//!    the same serde round-trip so `skip_serializing_if` and `default`
//!    attributes cannot drift from the schema either.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use jsonschema::{Registry, Validator};
use protocol::{
    CreatePageRequest, HealthResponse, LibraryEvent, ListPagesResponse, MeResponse, MetaResponse,
    PageClientMessage, PageReplay, PageServerMessage, PageSummary, Paper, RealtimeTicketResponse,
    Stroke, StrokeBatch, ThumbnailMetadata, TombstoneBatch,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

/// The `$id` prefix every schema declares; `$ref`s between schemas resolve
/// relative to it.
const SCHEMA_BASE: &str = "https://v-note/schemas/";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

fn read_json(path: &Path) -> Value {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("{} should be readable: {e}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("{} is not valid JSON: {e}", path.display()))
}

fn json_files(dir: &Path) -> Vec<(String, Value)> {
    let mut out: Vec<(String, Value)> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{} should be listable: {e}", dir.display()))
        .map(|entry| entry.expect("directory entry").path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
        .map(|path| {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .expect("utf-8 file name")
                .to_string();
            (name, read_json(&path))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    assert!(!out.is_empty(), "no .json files under {}", dir.display());
    out
}

fn schemas() -> Vec<(String, Value)> {
    json_files(&repo_root().join("schemas"))
}

fn fixtures() -> Vec<(String, Value)> {
    json_files(&repo_root().join("contracts").join("fixtures"))
}

/// All schemas registered under their canonical `$id` URIs so cross-schema
/// `$ref`s (`"$ref": "stroke.schema.json"`) resolve without touching the
/// network or the file system.
fn registry(schemas: &[(String, Value)]) -> Registry<'static> {
    let mut builder = Registry::new();
    for (name, schema) in schemas {
        builder = builder
            .add(format!("{SCHEMA_BASE}{name}"), schema.clone())
            .unwrap_or_else(|e| panic!("registering {name}: {e}"));
    }
    builder
        .prepare()
        .expect("every $ref between schemas should resolve to a registered schema")
}

fn validator(registry: &Registry<'_>, schema: &str) -> Validator {
    let root = json!({ "$ref": format!("{SCHEMA_BASE}{schema}") });
    jsonschema::options()
        .with_registry(registry)
        .build(&root)
        .unwrap_or_else(|e| panic!("building validator for {schema}: {e}"))
}

/// Validation failures as one line each, or empty when the instance is valid.
fn errors(validator: &Validator, instance: &Value) -> Vec<String> {
    validator
        .iter_errors(instance)
        .map(|error| format!("{} (at {})", error, error.instance_path()))
        .collect()
}

/// Which schema a fixture is asserted against. Every fixture must be listed:
/// an unmapped fixture is a hole in the contract, not an oversight to skip.
fn schema_for_fixture(fixture: &str) -> Option<&'static str> {
    Some(match fixture {
        "create-page.json" => "create-page.schema.json",
        "health.json" => "health.schema.json",
        "me.json" => "me.schema.json",
        "meta.json" => "meta.schema.json",
        "page.json" => "page.schema.json",
        "pages.json" => "pages.schema.json",
        "page-replay.json" => "page-replay.schema.json",
        // Note: `page-server-page-replay.json` is matched by the
        // `page-server-` prefix rule below, as the wire *message*.
        "realtime-ticket.json" => "realtime-ticket.schema.json",
        "stroke.json" | "stroke-pressure.json" => "stroke.schema.json",
        "stroke-batch.json" => "stroke-batch.schema.json",
        // Golden geometry table, not a wire payload: asserted by contracts.rs
        // and the Android PaperGeometryTest.
        "paper-geometry.json" => return None,
        name if name.starts_with("library-event-") => "library-event.schema.json",
        name if name.starts_with("page-client-") => "page-client-message.schema.json",
        name if name.starts_with("page-server-") => "page-server-message.schema.json",
        other => panic!(
            "fixture {other} has no schema mapping — add it to schema_for_fixture \
             (or list it there as golden-only)"
        ),
    })
}

#[test]
fn every_schema_is_a_valid_2020_12_schema() {
    for (name, schema) in schemas() {
        assert_eq!(
            schema.get("$schema").and_then(Value::as_str),
            Some("https://json-schema.org/draft/2020-12/schema"),
            "{name} must declare the 2020-12 draft"
        );
        if let Err(error) = jsonschema::meta::validate(&schema) {
            panic!("{name} is not a valid JSON Schema: {error}");
        }
    }
}

#[test]
fn every_fixture_validates_against_its_schema() {
    let schemas = schemas();
    let registry = registry(&schemas);
    let mut failures = Vec::new();
    let mut checked = 0;
    for (name, fixture) in fixtures() {
        let Some(schema) = schema_for_fixture(&name) else {
            continue;
        };
        let validator = validator(&registry, schema);
        for error in errors(&validator, &fixture) {
            failures.push(format!("{name} vs {schema}: {error}"));
        }
        checked += 1;
    }
    assert!(
        checked >= 20,
        "only {checked} fixtures checked — wrong directory?"
    );
    assert!(
        failures.is_empty(),
        "fixtures drifted from schemas/:\n  {}",
        failures.join("\n  ")
    );
}

/// Deserialise a fixture into its Rust type, serialise it back, and validate
/// what serde actually emits. The fixture check above proves the hand-written
/// JSON matches the schema; this proves the code does.
fn assert_roundtrip<T: DeserializeOwned + Serialize>(
    registry: &Registry<'_>,
    fixture: &str,
    schema: &str,
) {
    let value = read_json(&repo_root().join("contracts").join("fixtures").join(fixture));
    let parsed: T = serde_json::from_value(value)
        .unwrap_or_else(|e| panic!("{fixture} should deserialise as {}: {e}", type_name::<T>()));
    let emitted = serde_json::to_value(&parsed).expect("serialisable");
    let failures = errors(&validator(registry, schema), &emitted);
    assert!(
        failures.is_empty(),
        "serde output for {} (from {fixture}) does not match {schema}:\n  {}",
        type_name::<T>(),
        failures.join("\n  ")
    );
}

fn type_name<T>() -> &'static str {
    std::any::type_name::<T>()
        .rsplit("::")
        .next()
        .unwrap_or("?")
}

#[test]
fn rest_dtos_serialise_to_their_schemas() {
    let schemas = schemas();
    let registry = registry(&schemas);
    assert_roundtrip::<HealthResponse>(&registry, "health.json", "health.schema.json");
    assert_roundtrip::<MetaResponse>(&registry, "meta.json", "meta.schema.json");
    assert_roundtrip::<MeResponse>(&registry, "me.json", "me.schema.json");
    assert_roundtrip::<PageSummary>(&registry, "page.json", "page.schema.json");
    assert_roundtrip::<ListPagesResponse>(&registry, "pages.json", "pages.schema.json");
    assert_roundtrip::<CreatePageRequest>(&registry, "create-page.json", "create-page.schema.json");
    assert_roundtrip::<RealtimeTicketResponse>(
        &registry,
        "realtime-ticket.json",
        "realtime-ticket.schema.json",
    );
    assert_roundtrip::<PageReplay>(&registry, "page-replay.json", "page-replay.schema.json");
    assert_roundtrip::<StrokeBatch>(&registry, "stroke-batch.json", "stroke-batch.schema.json");
    assert_roundtrip::<Stroke>(&registry, "stroke.json", "stroke.schema.json");
    assert_roundtrip::<Stroke>(&registry, "stroke-pressure.json", "stroke.schema.json");
}

// ---- Tagged message enums: one sample per variant --------------------------

fn sample_stroke() -> Stroke {
    let value = read_json(
        &repo_root()
            .join("contracts")
            .join("fixtures")
            .join("stroke-pressure.json"),
    );
    serde_json::from_value(value).expect("stroke-pressure.json should parse")
}

fn sample_batch() -> StrokeBatch {
    StrokeBatch {
        seq: 7,
        client_batch_id: "batch_sample".to_string(),
        strokes: vec![sample_stroke()],
    }
}

fn sample_tombstones() -> TombstoneBatch {
    TombstoneBatch {
        revision: 3,
        client_mutation_id: "mutation_sample".to_string(),
        stroke_ids: vec!["stroke_pressure_fixture_1".to_string()],
    }
}

fn sample_replay() -> PageReplay {
    PageReplay {
        page_id: "page_01j00000000000000000000000".to_string(),
        last_seq: 7,
        batches: vec![sample_batch()],
        tombstones: vec![sample_tombstones()],
    }
}

fn sample_page(thumbnail: ThumbnailMetadata) -> PageSummary {
    PageSummary {
        id: "page_01j00000000000000000000000".to_string(),
        title: "Sample".to_string(),
        created_at: "2026-09-15T00:00:00Z".to_string(),
        updated_at: "2026-09-15T00:00:00Z".to_string(),
        thumbnail,
        paper: Paper::RuledMarginNarrow,
    }
}

fn thumbnail_states() -> Vec<ThumbnailMetadata> {
    vec![
        ThumbnailMetadata::Empty,
        ThumbnailMetadata::Generating { source_seq: 1 },
        ThumbnailMetadata::Available {
            source_seq: 1,
            url: "/api/pages/page_01j00000000000000000000000/thumbnails/1".to_string(),
        },
        ThumbnailMetadata::Failed { source_seq: 1 },
    ]
}

fn server_samples() -> Vec<PageServerMessage> {
    let samples = vec![
        PageServerMessage::Welcome {
            session_id: "session_a".to_string(),
            last_seq: 7,
            lease_holder: Some("session_b".to_string()),
            paper: Paper::SquaredSmall,
        },
        PageServerMessage::Welcome {
            session_id: "session_a".to_string(),
            last_seq: 0,
            lease_holder: None,
            paper: Paper::None,
        },
        PageServerMessage::StrokeBatch(sample_batch()),
        PageServerMessage::PageReplay(sample_replay()),
        PageServerMessage::PageReplay(PageReplay {
            page_id: "page_01j00000000000000000000000".to_string(),
            last_seq: 0,
            batches: Vec::new(),
            tombstones: Vec::new(),
        }),
        PageServerMessage::TombstoneBatch(sample_tombstones()),
        PageServerMessage::Synced { last_seq: 7 },
        PageServerMessage::LeaseGranted,
        PageServerMessage::LeaseDenied {
            holder: "session_b".to_string(),
        },
        PageServerMessage::PaperChanged {
            paper: Paper::RuledWide,
            revision: 2,
        },
        PageServerMessage::LeaseChanged {
            holder: Some("session_b".to_string()),
        },
        PageServerMessage::LeaseChanged { holder: None },
        PageServerMessage::Error {
            code: "batch_rejected".to_string(),
            message: "stroke width out of range".to_string(),
            client_mutation_id: Some("mutation_sample".to_string()),
        },
        PageServerMessage::Error {
            code: "internal".to_string(),
            message: "boom".to_string(),
            client_mutation_id: None,
        },
    ];
    // Adding a variant to the enum fails to compile here until a sample for
    // it is added above.
    for sample in &samples {
        match sample {
            PageServerMessage::Welcome { .. }
            | PageServerMessage::StrokeBatch(_)
            | PageServerMessage::PageReplay(_)
            | PageServerMessage::TombstoneBatch(_)
            | PageServerMessage::Synced { .. }
            | PageServerMessage::LeaseGranted
            | PageServerMessage::LeaseDenied { .. }
            | PageServerMessage::PaperChanged { .. }
            | PageServerMessage::LeaseChanged { .. }
            | PageServerMessage::Error { .. } => {}
        }
    }
    samples
}

fn client_samples() -> Vec<PageClientMessage> {
    let samples = vec![
        PageClientMessage::Subscribe { from_seq: 0 },
        PageClientMessage::AcquireLease,
        PageClientMessage::RenewLease,
        PageClientMessage::ReleaseLease,
        PageClientMessage::CommitBatch {
            client_batch_id: "batch_sample".to_string(),
            strokes: vec![sample_stroke()],
        },
        PageClientMessage::CommitTombstones {
            client_mutation_id: "mutation_sample".to_string(),
            stroke_ids: vec!["stroke_pressure_fixture_1".to_string()],
        },
        PageClientMessage::SetPaper {
            client_mutation_id: "mutation_sample".to_string(),
            paper: Paper::SquaredLarge,
        },
    ];
    for sample in &samples {
        match sample {
            PageClientMessage::Subscribe { .. }
            | PageClientMessage::AcquireLease
            | PageClientMessage::RenewLease
            | PageClientMessage::ReleaseLease
            | PageClientMessage::CommitBatch { .. }
            | PageClientMessage::CommitTombstones { .. }
            | PageClientMessage::SetPaper { .. } => {}
        }
    }
    samples
}

fn library_samples() -> Vec<LibraryEvent> {
    let page_id = "page_01j00000000000000000000000".to_string();
    let mut samples = vec![
        LibraryEvent::PageCreated {
            page: sample_page(ThumbnailMetadata::Empty),
        },
        LibraryEvent::PageDeleted {
            page_id: page_id.clone(),
        },
        LibraryEvent::PageUpdated {
            page_id: page_id.clone(),
            updated_at: "2026-09-15T00:00:00Z".to_string(),
        },
    ];
    samples.extend(thumbnail_states().into_iter().map(|thumbnail| {
        LibraryEvent::PageThumbnailUpdated {
            page_id: page_id.clone(),
            thumbnail,
        }
    }));
    for sample in &samples {
        match sample {
            LibraryEvent::PageCreated { .. }
            | LibraryEvent::PageDeleted { .. }
            | LibraryEvent::PageThumbnailUpdated { .. }
            | LibraryEvent::PageUpdated { .. } => {}
        }
    }
    samples
}

/// The `type` constants a tagged-union schema's `oneOf` branches accept.
fn schema_type_tags(schema: &Value) -> BTreeSet<String> {
    schema
        .get("oneOf")
        .and_then(Value::as_array)
        .expect("tagged-union schema should be a oneOf")
        .iter()
        .map(|branch| {
            branch
                .pointer("/properties/type/const")
                .and_then(Value::as_str)
                .expect("every oneOf branch should pin `type` with a const")
                .to_string()
        })
        .collect()
}

fn assert_variants_match_schema<T: Serialize>(
    schemas: &[(String, Value)],
    schema: &str,
    samples: &[T],
    message_type: impl Fn(&T) -> &'static str,
) {
    let registry = registry(schemas);
    let validator = validator(&registry, schema);
    let mut failures = Vec::new();
    let mut seen = BTreeSet::new();
    for sample in samples {
        let tag = message_type(sample);
        seen.insert(tag.to_string());
        let emitted = serde_json::to_value(sample).expect("serialisable");
        for error in errors(&validator, &emitted) {
            failures.push(format!("{tag}: {error}\n    payload: {emitted}"));
        }
    }
    assert!(
        failures.is_empty(),
        "serialised variants do not match {schema}:\n  {}",
        failures.join("\n  ")
    );
    let (_, schema_value) = schemas
        .iter()
        .find(|(name, _)| name == schema)
        .unwrap_or_else(|| panic!("{schema} should exist"));
    let declared = schema_type_tags(schema_value);
    assert_eq!(
        declared, seen,
        "{schema} oneOf branches (left) must cover exactly the enum variants (right)"
    );
}

#[test]
fn page_server_message_variants_match_schema() {
    assert_variants_match_schema(
        &schemas(),
        "page-server-message.schema.json",
        &server_samples(),
        PageServerMessage::message_type,
    );
}

#[test]
fn page_client_message_variants_match_schema() {
    assert_variants_match_schema(
        &schemas(),
        "page-client-message.schema.json",
        &client_samples(),
        PageClientMessage::message_type,
    );
}

#[test]
fn library_event_variants_match_schema() {
    assert_variants_match_schema(
        &schemas(),
        "library-event.schema.json",
        &library_samples(),
        LibraryEvent::message_type,
    );
}
