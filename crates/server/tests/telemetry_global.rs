//! The production subscriber stack installed the way `init_tracing` installs
//! it: once, as the process-wide global default (#417).
//!
//! The unit tests in `observability/tests.rs` run the same stack as a
//! thread-local default, which is not equivalent: interest caching and max
//! level behave differently under a global dispatcher. Pipeline 417 shipped an
//! OTLP-layer filter that passed every unit test and yet, in the deployed
//! binary, `page created` and `stroke batch committed` went missing from stdout
//! as well as OTLP. This binary is its own process, so it installs the stack
//! the production way (`try_init`, as `init_tracing`'s `.init()` does) and
//! asserts what each path received. (It does not reproduce 417's fault — see
//! `WithoutExporterInternals` — but it holds the production installation to
//! the contract the unit tests assert on the thread-local one.)

use std::sync::{Arc, Mutex};

use opentelemetry::logs::AnyValue;
use opentelemetry_sdk::logs::{InMemoryLogExporter, SdkLoggerProvider};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
use server::observability::{LogFilters, telemetry_subscriber};

#[derive(Clone, Default)]
struct Stdout(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Stdout {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("stdout lock").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Stdout {
    fn messages(&self) -> Vec<serde_json::Value> {
        String::from_utf8(self.0.lock().expect("stdout lock").clone())
            .expect("utf-8")
            .lines()
            .map(|line| serde_json::from_str(line).expect("JSON line"))
            .collect()
    }
}

/// Shaped like `create_page`: an instrumented handler with a field recorded
/// after entry, emitting an event with Display, `&str` and bool fields.
#[tracing::instrument(skip_all, fields(owner_id = "owner_1", page_id))]
async fn mutation(page_id: &str) {
    tracing::Span::current().record("page_id", page_id);
    tokio::task::yield_now().await;
    tracing::info!(page_id = %page_id, paper = "none", titled = true, "page created");
    tracing::info!(
        page_id,
        session_id = "session_1",
        client_batch_id = "batch_1",
        seq = 5_u64,
        revision = 9_u64,
        submitted = 2_usize,
        visible = 1_usize,
        thumbnail_queued = false,
        "stroke batch committed"
    );
}

#[test]
fn the_globally_installed_stack_delivers_every_line_to_both_paths() {
    let spans = InMemorySpanExporter::default();
    let logs = InMemoryLogExporter::default();
    let tracer_provider = SdkTracerProvider::builder()
        .with_simple_exporter(spans)
        .build();
    let logger_provider = SdkLoggerProvider::builder()
        .with_simple_exporter(logs.clone())
        .build();
    let stdout = Stdout::default();
    // The production default filter, plus this binary's own target (it is not
    // `server`); otlp-log-filter unset, so the OTLP layer uses the same one.
    let filters = LogFilters::new(
        Some("server=info,tower_http=info,axum=info,opentelemetry=warn,telemetry_global=info"),
        None,
    );

    tracing_subscriber::util::SubscriberInitExt::try_init(telemetry_subscriber(
        &filters,
        {
            let stdout = stdout.clone();
            move || stdout.clone()
        },
        Some(&tracer_provider),
        Some(&logger_provider),
    ))
    .expect("the only global subscriber in this binary");

    tracing::info!("startup line");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        tokio::spawn(mutation("page_1"))
            .await
            .expect("mutation task");
    });
    tracing::warn!(target: "opentelemetry_sdk", "export failed");

    let stdout = stdout.messages();
    let exported: Vec<AnyValue> = logs
        .get_emitted_logs()
        .expect("logs")
        .into_iter()
        .filter_map(|log| log.record.body().cloned())
        .collect();
    for message in ["startup line", "page created", "stroke batch committed"] {
        assert!(
            stdout.iter().any(|line| line["message"] == message),
            "stdout is missing {message:?}: {stdout:?}"
        );
        assert!(
            exported.contains(&AnyValue::String(message.to_string().into())),
            "OTLP is missing {message:?}: {exported:?}"
        );
    }
    let created = stdout
        .iter()
        .find(|line| line["message"] == "page created")
        .expect("page created");
    assert!(created["trace_id"].is_string(), "{created}");
    assert!(
        stdout.iter().any(|line| line["message"] == "export failed"),
        "the SDK's own report stays on stdout"
    );
    assert!(
        !exported.contains(&AnyValue::String("export failed".into())),
        "and never reaches OTLP"
    );
}
