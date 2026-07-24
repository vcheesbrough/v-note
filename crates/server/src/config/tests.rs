//! Per-DTO config tests.
//!
//! These drive the real layering (in-memory defaults + the `VNOTE__` environment
//! source) but inject the environment map explicitly rather than mutating the
//! process environment, so the tests stay deterministic under parallel execution.

use std::collections::HashMap;

use super::*;

/// Build a `config::Config` from the real defaults plus an injected env map,
/// exactly as `build_config` does minus the sovereign-config layer.
fn cfg(entries: &[(&str, &str)]) -> Config {
    let source: HashMap<String, String> = entries
        .iter()
        .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
        .collect();
    apply_defaults(Config::builder())
        .expect("defaults should apply")
        .add_source(env_source().source(Some(source)))
        .build()
        .expect("config should build")
}

fn database_env() -> Vec<(&'static str, &'static str)> {
    vec![
        ("VNOTE__DATABASE__HOST", "postgres"),
        ("VNOTE__DATABASE__NAME", "v_note"),
        ("VNOTE__DATABASE__USER", "v_note"),
        ("VNOTE__DATABASE__PASSWORD", "s3cret"),
    ]
}

fn oidc_env() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "VNOTE__OIDC__ISSUER-URL",
            "https://auth.example/app/o/v-note/",
        ),
        ("VNOTE__OIDC__CLIENT-ID", "v-note-browser"),
        ("VNOTE__OIDC__CLIENT-SECRET", "shhh"),
        (
            "VNOTE__OIDC__REDIRECT-URI",
            "https://v-notes.example/auth/callback",
        ),
        ("VNOTE__OIDC__REQUIRED-SCOPE", "v-note:prod:access"),
    ]
}

fn with(
    base: Vec<(&'static str, &'static str)>,
    extra: &[(&'static str, &'static str)],
) -> Vec<(&'static str, &'static str)> {
    let mut all = base;
    all.extend_from_slice(extra);
    all
}

// ---------------------------------------------------------------------------
// database
// ---------------------------------------------------------------------------

#[test]
fn database_maps_kebab_keys_and_coerces_port_default() {
    let config = cfg(&database_env());
    let database: DatabaseConfig =
        load_group(&config, "database").expect("database group should load");

    assert_eq!(database.host, "postgres");
    assert_eq!(database.name, "v_note");
    assert_eq!(database.user, "v_note");
    assert_eq!(database.password, "s3cret");
    // Text leaf from the defaults layer coerced into a real u16.
    assert_eq!(database.port, 5432);
}

#[test]
fn database_port_coerces_from_text_leaf() {
    let config = cfg(&with(database_env(), &[("VNOTE__DATABASE__PORT", "6543")]));
    let database: DatabaseConfig = load_group(&config, "database").expect("should load");

    assert_eq!(database.port, 6543);
}

#[test]
fn database_port_that_is_not_a_u16_fails_to_deserialize() {
    let config = cfg(&with(
        database_env(),
        &[("VNOTE__DATABASE__PORT", "not-a-port")],
    ));
    let error = load_group::<DatabaseConfig>(&config, "database")
        .expect_err("non-numeric port should be rejected");

    assert!(
        matches!(error, ConfigError::Load { ref group, .. } if group == "database"),
        "expected a Load error, got: {error}"
    );
}

#[test]
fn database_missing_required_field_is_rejected() {
    // No password leaf anywhere.
    let config = cfg(&[
        ("VNOTE__DATABASE__HOST", "postgres"),
        ("VNOTE__DATABASE__NAME", "v_note"),
        ("VNOTE__DATABASE__USER", "v_note"),
    ]);
    let error = load_group::<DatabaseConfig>(&config, "database")
        .expect_err("missing password should be rejected");

    assert!(matches!(error, ConfigError::Load { .. }));
    assert!(
        error.to_string().contains("password"),
        "error should name the missing field: {error}"
    );
}

#[test]
fn database_blank_password_is_rejected_by_validate() {
    let config = cfg(&with(
        database_env(),
        &[("VNOTE__DATABASE__PASSWORD", "   ")],
    ));
    let error = load_group::<DatabaseConfig>(&config, "database")
        .expect_err("blank password should be rejected");

    match error {
        ConfigError::Invalid { ref path, .. } => assert_eq!(path, "database.password"),
        other => panic!("expected Invalid, got: {other}"),
    }
    // The offending value must never appear in the message.
    assert!(!error.to_string().contains("   "));
}

// ---------------------------------------------------------------------------
// oidc
// ---------------------------------------------------------------------------

#[test]
fn oidc_maps_kebab_keys_into_rich_types() {
    let config = cfg(&oidc_env());
    let oidc: OidcConfig = load_group(&config, "oidc").expect("oidc group should load");

    assert_eq!(
        oidc.issuer_url.as_str(),
        "https://auth.example/app/o/v-note/"
    );
    assert_eq!(oidc.client_id, "v-note-browser");
    assert_eq!(oidc.client_secret, "shhh");
    assert_eq!(
        oidc.redirect_uri.as_str(),
        "https://v-notes.example/auth/callback"
    );
    assert_eq!(oidc.required_scope, "v-note:prod:access");
    // Optional leaves absent → None, not an error.
    assert!(oidc.authorize_url.is_none());
    assert!(oidc.end_session_url.is_none());
    assert!(oidc.android.client_id.is_none());
    assert!(oidc.android.issuer_url.is_none());
}

#[test]
fn oidc_nested_android_subtree_deserializes() {
    let config = cfg(&with(
        oidc_env(),
        &[
            ("VNOTE__OIDC__ANDROID__CLIENT-ID", "v-note-android"),
            (
                "VNOTE__OIDC__ANDROID__ISSUER-URL",
                "https://auth.example/app/o/v-note-android/",
            ),
        ],
    ));
    let oidc: OidcConfig = load_group(&config, "oidc").expect("should load");

    assert_eq!(oidc.android.client_id.as_deref(), Some("v-note-android"));
    assert_eq!(
        oidc.android.issuer_url.expect("issuer url").as_str(),
        "https://auth.example/app/o/v-note-android/"
    );
}

#[test]
fn oidc_blank_optional_leaves_are_treated_as_absent() {
    // Deploy tooling renders unset values as empty strings; blank must mean
    // "absent", not "invalid URL", or the server would refuse to start.
    let config = cfg(&with(
        oidc_env(),
        &[
            ("VNOTE__OIDC__AUTHORIZE-URL", ""),
            ("VNOTE__OIDC__END-SESSION-URL", "   "),
            ("VNOTE__OIDC__ANDROID__CLIENT-ID", ""),
            ("VNOTE__OIDC__ANDROID__ISSUER-URL", ""),
        ],
    ));
    let oidc: OidcConfig = load_group(&config, "oidc").expect("blank optionals should load");

    assert!(oidc.authorize_url.is_none());
    assert!(oidc.end_session_url.is_none());
    assert!(oidc.android.client_id.is_none());
    assert!(oidc.android.issuer_url.is_none());
}

#[test]
fn observability_blank_otlp_endpoint_disables_export() {
    let config = cfg(&[
        ("VNOTE__OBSERVABILITY__ENVIRONMENT", "dev"),
        ("VNOTE__OBSERVABILITY__OTLP-ENDPOINT", ""),
    ]);
    let observability: ObservabilityConfig =
        load_group(&config, "observability").expect("blank endpoint should load");

    assert!(observability.otlp_endpoint.is_none());
}

#[test]
fn oidc_malformed_url_fails_to_deserialize() {
    let config = cfg(&with(
        oidc_env(),
        &[("VNOTE__OIDC__ISSUER-URL", "not a url")],
    ));
    let error =
        load_group::<OidcConfig>(&config, "oidc").expect_err("malformed URL should be rejected");

    assert!(
        matches!(error, ConfigError::Load { ref group, .. } if group == "oidc"),
        "expected a Load error, got: {error}"
    );
}

#[test]
fn oidc_missing_client_id_is_rejected() {
    let without_client_id: Vec<_> = oidc_env()
        .into_iter()
        .filter(|(key, _)| *key != "VNOTE__OIDC__CLIENT-ID")
        .collect();
    let error = load_group::<OidcConfig>(&cfg(&without_client_id), "oidc")
        .expect_err("missing client-id should be rejected");

    assert!(matches!(error, ConfigError::Load { .. }));
    assert!(
        error.to_string().contains("client-id"),
        "error should name the missing leaf: {error}"
    );
}

#[test]
fn oidc_blank_client_secret_is_rejected_without_leaking_it() {
    let config = cfg(&with(oidc_env(), &[("VNOTE__OIDC__CLIENT-SECRET", "  ")]));
    let error = load_group::<OidcConfig>(&config, "oidc").expect_err("blank secret rejected");

    match error {
        ConfigError::Invalid {
            ref path,
            ref reason,
        } => {
            assert_eq!(path, "oidc.client-secret");
            assert_eq!(reason, "must not be empty");
        }
        other => panic!("expected Invalid, got: {other}"),
    }
}

#[test]
fn oidc_blank_required_scope_is_rejected() {
    let config = cfg(&with(oidc_env(), &[("VNOTE__OIDC__REQUIRED-SCOPE", "")]));
    let error = load_group::<OidcConfig>(&config, "oidc").expect_err("blank scope rejected");

    match error {
        ConfigError::Invalid { ref path, .. } => assert_eq!(path, "oidc.required-scope"),
        other => panic!("expected Invalid, got: {other}"),
    }
}

// ---------------------------------------------------------------------------
// observability
// ---------------------------------------------------------------------------

#[test]
fn observability_defaults_apply_when_only_environment_is_set() {
    let config = cfg(&[("VNOTE__OBSERVABILITY__ENVIRONMENT", "production")]);
    let observability: ObservabilityConfig =
        load_group(&config, "observability").expect("should load");

    assert_eq!(observability.environment, "production");
    assert_eq!(observability.otlp_protocol, "grpc");
    assert_eq!(observability.otlp_timeout_ms, 2000);
    assert_eq!(observability.service_name, "v-note");
    assert_eq!(
        observability.metrics_socket_addr(),
        Some("0.0.0.0:9090".parse().expect("default addr"))
    );
    // No endpoint configured → OTLP export stays off.
    assert!(observability.otlp_endpoint.is_none());
    assert_eq!(observability.otlp_timeout().as_millis(), 2000);
}

#[test]
fn observability_environment_overrides_defaults() {
    let config = cfg(&[
        ("VNOTE__OBSERVABILITY__ENVIRONMENT", "dev"),
        ("VNOTE__OBSERVABILITY__OTLP-ENDPOINT", "http://alloy:4317/"),
        ("VNOTE__OBSERVABILITY__OTLP-TIMEOUT-MS", "5000"),
        ("VNOTE__OBSERVABILITY__SERVICE-NAME", "v-note-dev"),
        ("VNOTE__OBSERVABILITY__METRICS-ADDR", "127.0.0.1:9111"),
    ]);
    let observability: ObservabilityConfig =
        load_group(&config, "observability").expect("should load");

    assert_eq!(
        observability
            .otlp_endpoint
            .as_ref()
            .expect("endpoint")
            .as_str(),
        "http://alloy:4317/"
    );
    assert_eq!(observability.otlp_timeout_ms, 5000);
    assert_eq!(observability.service_name, "v-note-dev");
    assert_eq!(
        observability.metrics_socket_addr(),
        Some("127.0.0.1:9111".parse().expect("addr"))
    );
}

#[test]
fn observability_missing_environment_is_rejected() {
    let error = load_group::<ObservabilityConfig>(&cfg(&[]), "observability")
        .expect_err("missing environment should be rejected");

    assert!(matches!(error, ConfigError::Load { .. }));
    assert!(
        error.to_string().contains("environment"),
        "error should name the missing leaf: {error}"
    );
}

#[test]
fn observability_non_numeric_timeout_fails_to_deserialize() {
    let config = cfg(&[
        ("VNOTE__OBSERVABILITY__ENVIRONMENT", "dev"),
        ("VNOTE__OBSERVABILITY__OTLP-TIMEOUT-MS", "soon"),
    ]);
    let error = load_group::<ObservabilityConfig>(&config, "observability")
        .expect_err("non-numeric timeout should be rejected");

    assert!(matches!(error, ConfigError::Load { .. }));
}

#[test]
fn observability_rejects_unsupported_otlp_protocol() {
    let config = cfg(&[
        ("VNOTE__OBSERVABILITY__ENVIRONMENT", "dev"),
        ("VNOTE__OBSERVABILITY__OTLP-PROTOCOL", "http/protobuf"),
    ]);
    let error = load_group::<ObservabilityConfig>(&config, "observability")
        .expect_err("only grpc is supported");

    match error {
        ConfigError::Invalid { ref path, .. } => {
            assert_eq!(path, "observability.otlp-protocol");
        }
        other => panic!("expected Invalid, got: {other}"),
    }
}

#[test]
fn observability_rejects_malformed_metrics_addr() {
    let config = cfg(&[
        ("VNOTE__OBSERVABILITY__ENVIRONMENT", "dev"),
        ("VNOTE__OBSERVABILITY__METRICS-ADDR", "nine thousand"),
    ]);
    let error = load_group::<ObservabilityConfig>(&config, "observability")
        .expect_err("malformed metrics addr should be rejected");

    match error {
        ConfigError::Invalid { ref path, .. } => {
            assert_eq!(path, "observability.metrics-addr");
        }
        other => panic!("expected Invalid, got: {other}"),
    }
}

#[test]
fn observability_metrics_can_be_disabled() {
    for value in ["disabled", "DISABLED", ""] {
        let config = cfg(&[
            ("VNOTE__OBSERVABILITY__ENVIRONMENT", "dev"),
            ("VNOTE__OBSERVABILITY__METRICS-ADDR", value),
        ]);
        let observability: ObservabilityConfig =
            load_group(&config, "observability").expect("disabled metrics is valid");

        assert_eq!(
            observability.metrics_socket_addr(),
            None,
            "metrics-addr={value:?} should disable the listener"
        );
    }
}

// ---------------------------------------------------------------------------
// android
// ---------------------------------------------------------------------------

#[test]
fn android_defaults_to_unconfigured_assetlinks() {
    let android: AndroidConfig = load_group(&cfg(&[]), "android").expect("android group defaults");

    assert!(android.assetlinks_json().is_none());
}

#[test]
fn android_accepts_valid_assetlinks_json() {
    let json = r#"[{"relation":["delegate_permission/common.handle_all_urls"]}]"#;
    let config = cfg(&[("VNOTE__ANDROID__ASSETLINKS-JSON", json)]);
    let android: AndroidConfig = load_group(&config, "android").expect("valid JSON should load");

    assert_eq!(android.assetlinks_json(), Some(json));
}

#[test]
fn android_rejects_malformed_assetlinks_json() {
    let config = cfg(&[("VNOTE__ANDROID__ASSETLINKS-JSON", "{not json")]);
    let error =
        load_group::<AndroidConfig>(&config, "android").expect_err("malformed JSON rejected");

    match error {
        ConfigError::Invalid {
            ref path,
            ref reason,
        } => {
            assert_eq!(path, "android.assetlinks-json");
            assert_eq!(reason, "must be valid JSON");
        }
        other => panic!("expected Invalid, got: {other}"),
    }
}

// ---------------------------------------------------------------------------
// server
// ---------------------------------------------------------------------------

#[test]
fn server_defaults_to_http_on_8080_with_no_tls_or_static() {
    let server: ServerConfig = load_group(&cfg(&[]), "server").expect("server group defaults");

    assert_eq!(server.http_port, 8080);
    assert!(server.tls_pair().is_none());
    assert!(server.static_dir.is_none());
}

#[test]
fn server_maps_kebab_keys_and_coerces_port() {
    let config = cfg(&[
        ("VNOTE__SERVER__HTTP-PORT", "9000"),
        ("VNOTE__SERVER__TLS-CERT", "/app/cert.pem"),
        ("VNOTE__SERVER__TLS-KEY", "/app/key.pem"),
        ("VNOTE__SERVER__STATIC-DIR", "/app/dist"),
    ]);
    let server: ServerConfig = load_group(&config, "server").expect("should load");

    assert_eq!(server.http_port, 9000);
    let (cert, key) = server.tls_pair().expect("tls pair present");
    assert_eq!(cert.as_os_str(), "/app/cert.pem");
    assert_eq!(key.as_os_str(), "/app/key.pem");
    assert_eq!(
        server.static_dir.as_deref().map(|p| p.to_str().unwrap()),
        Some("/app/dist")
    );
}

#[test]
fn server_non_numeric_port_fails_to_deserialize() {
    let config = cfg(&[("VNOTE__SERVER__HTTP-PORT", "https")]);
    let error =
        load_group::<ServerConfig>(&config, "server").expect_err("non-numeric port rejected");

    assert!(matches!(error, ConfigError::Load { .. }));
}

#[test]
fn server_tls_cert_without_key_is_rejected() {
    let config = cfg(&[("VNOTE__SERVER__TLS-CERT", "/app/cert.pem")]);
    let error = load_group::<ServerConfig>(&config, "server").expect_err("lone cert rejected");

    match error {
        ConfigError::Invalid { ref path, .. } => assert_eq!(path, "server.tls-key"),
        other => panic!("expected Invalid, got: {other}"),
    }
}

#[test]
fn server_tls_key_without_cert_is_rejected() {
    let config = cfg(&[("VNOTE__SERVER__TLS-KEY", "/app/key.pem")]);
    let error = load_group::<ServerConfig>(&config, "server").expect_err("lone key rejected");

    match error {
        ConfigError::Invalid { ref path, .. } => assert_eq!(path, "server.tls-cert"),
        other => panic!("expected Invalid, got: {other}"),
    }
}

#[test]
fn server_blank_tls_and_static_are_treated_as_absent() {
    // The image sets these; a bare/local run leaves them blank → plain HTTP, no SPA.
    let config = cfg(&[
        ("VNOTE__SERVER__TLS-CERT", ""),
        ("VNOTE__SERVER__TLS-KEY", ""),
        ("VNOTE__SERVER__STATIC-DIR", "   "),
    ]);
    let server: ServerConfig = load_group(&config, "server").expect("blank optionals load");

    assert!(server.tls_pair().is_none());
    assert!(server.static_dir.is_none());
}

// ---------------------------------------------------------------------------
// secret redaction
// ---------------------------------------------------------------------------

/// `ConfigError::Load` forwards `config`'s own message, which embeds the offending
/// value on a type mismatch (`invalid type: string "…", expected an integer`). What
/// keeps a secret out of that message is that every secret leaf is `String`-typed,
/// and `String` cannot fail coercion.
///
/// That invariant is what this test pins: re-typing a secret leaf as anything richer
/// would put its value straight into the startup log.
#[test]
fn secret_leaves_never_leak_their_value_in_errors() {
    const SENTINEL: &str = "s3cr3t-sentinel-value-not-a-number";

    // database/password: a value that would break any richer type still loads,
    // proving the leaf is String-typed and so cannot produce a coercion error...
    let config = cfg(&with(
        database_env(),
        &[("VNOTE__DATABASE__PASSWORD", SENTINEL)],
    ));
    let database: DatabaseConfig =
        load_group(&config, "database").expect("a String secret accepts any value");
    assert_eq!(database.password, SENTINEL);

    // ...and when a *different* field in the same group fails to coerce, the
    // resulting error must not carry the secret alongside it.
    let config = cfg(&with(
        database_env(),
        &[
            ("VNOTE__DATABASE__PASSWORD", SENTINEL),
            ("VNOTE__DATABASE__PORT", "not-a-port"),
        ],
    ));
    let error = load_group::<DatabaseConfig>(&config, "database")
        .expect_err("non-numeric port should be rejected");
    assert!(
        !error.to_string().contains(SENTINEL),
        "database/password leaked into a Load error: {error}"
    );

    // Same for oidc/client-secret.
    let config = cfg(&with(
        oidc_env(),
        &[("VNOTE__OIDC__CLIENT-SECRET", SENTINEL)],
    ));
    let oidc: OidcConfig = load_group(&config, "oidc").expect("a String secret accepts any value");
    assert_eq!(oidc.client_secret, SENTINEL);

    let config = cfg(&with(
        oidc_env(),
        &[
            ("VNOTE__OIDC__CLIENT-SECRET", SENTINEL),
            ("VNOTE__OIDC__ISSUER-URL", "not a url"),
        ],
    ));
    let error =
        load_group::<OidcConfig>(&config, "oidc").expect_err("malformed URL should be rejected");
    assert!(
        !error.to_string().contains(SENTINEL),
        "oidc/client-secret leaked into a Load error: {error}"
    );
}

/// The redaction guarantee that *is* unconditional: checks I perform myself never
/// echo the value, only the field path.
#[test]
fn validate_errors_name_the_field_but_never_the_value() {
    const SENTINEL: &str = "s3cr3t-sentinel-value";

    let config = cfg(&with(oidc_env(), &[("VNOTE__OIDC__REQUIRED-SCOPE", "   ")]));
    let error = load_group::<OidcConfig>(&config, "oidc").expect_err("blank scope rejected");
    assert!(error.to_string().contains("oidc.required-scope"));
    assert!(!error.to_string().contains("   "));

    // A blank secret is reported by path alone.
    let config = cfg(&with(oidc_env(), &[("VNOTE__OIDC__CLIENT-SECRET", " ")]));
    let error = load_group::<OidcConfig>(&config, "oidc").expect_err("blank secret rejected");
    assert_eq!(
        error.to_string(),
        "invalid config `oidc.client-secret`: must not be empty"
    );
    assert!(!error.to_string().contains(SENTINEL));
}

// ---------------------------------------------------------------------------
// layering
// ---------------------------------------------------------------------------

#[test]
fn env_layer_overrides_the_defaults_layer_on_the_same_kebab_key() {
    // Same leaf, supplied by both layers: the env override must win, which only
    // holds if both layers produce the identical kebab-case key.
    let config = cfg(&[
        ("VNOTE__OBSERVABILITY__ENVIRONMENT", "dev"),
        ("VNOTE__OBSERVABILITY__SERVICE-NAME", "overridden"),
    ]);
    let observability: ObservabilityConfig = load_group(&config, "observability").expect("loads");

    assert_eq!(observability.service_name, "overridden");
}

#[test]
fn sovereign_source_is_disabled_without_an_access_url() {
    // Guards the local-dev / e2e path: no access URL in this test process, so the
    // sovereign layer must be skipped and config must come from defaults + env.
    assert!(!sovereign_source_enabled());
}
