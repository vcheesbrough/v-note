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
        // Wrapped exactly as `build_config` wraps it, so tests exercise trimming.
        .add_source(Trimmed(env_source().source(Some(source))))
        .build()
        .expect("config should build")
}

/// As [`cfg`], but with a stand-in for the sovereign-config layer between the
/// defaults and the env source — the same order, and the same `Trimmed` wrapper,
/// that `build_config` uses when an access URL is present. Keys are the canonical
/// dotted paths the real source emits (`observability.metrics-addr`).
fn cfg_with_sovereign(sovereign: &[(&str, &str)], env: &[(&str, &str)]) -> Config {
    let mut layer = Config::builder();
    for (key, value) in sovereign {
        layer = layer
            .set_override(*key, *value)
            .expect("sovereign fixture leaf should set");
    }
    let layer = layer.build().expect("sovereign fixture should build");

    let source: HashMap<String, String> = env
        .iter()
        .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
        .collect();
    apply_defaults(Config::builder())
        .expect("defaults should apply")
        .add_source(Trimmed(layer))
        .add_source(Trimmed(env_source().source(Some(source))))
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
    assert_eq!(
        oidc.redirect_uri.as_str(),
        "https://v-notes.example/auth/callback"
    );
    assert_eq!(oidc.required_scope, "v-note:prod:access");
    // Optional leaves absent → None, not an error.
    assert!(oidc.authorize_url.is_none());
    assert!(oidc.end_session_url.is_none());
}

/// #274 unified the SPA and Android clients, so `oidc/android/*` is gone. A
/// stale leaf left behind in a sovereign-config subtree must not fail the load —
/// the group has to stay deserializable through the rollout.
#[test]
fn oidc_ignores_retired_android_subtree() {
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
    let oidc: OidcConfig = load_group(&config, "oidc").expect("stale android leaves are ignored");

    assert_eq!(oidc.client_id, "v-note-browser");
}

/// The client secret is no longer part of the model: the unified client is
/// public (PKCE), so a leftover `oidc/client-secret` leaf is inert.
#[test]
fn oidc_ignores_retired_client_secret() {
    let config = cfg(&with(oidc_env(), &[("VNOTE__OIDC__CLIENT-SECRET", "shhh")]));
    let oidc: OidcConfig = load_group(&config, "oidc").expect("stale client-secret is ignored");

    assert_eq!(oidc.client_id, "v-note-browser");
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
        ],
    ));
    let oidc: OidcConfig = load_group(&config, "oidc").expect("blank optionals should load");

    assert!(oidc.authorize_url.is_none());
    assert!(oidc.end_session_url.is_none());
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
// realtime
// ---------------------------------------------------------------------------

/// The flag ships **on**: an operator has to ask for the old shape.
#[test]
fn realtime_coalesce_replay_defaults_to_on() {
    let realtime: RealtimeConfig =
        load_group(&cfg(&[]), "realtime").expect("realtime group defaults");

    assert!(realtime.coalesce_replay);
}

/// Every leaf is text in sovereign-config, so the rollback switch has to work
/// when written as a string — in either spelling of the env key.
#[test]
fn realtime_coalesce_replay_can_be_turned_off_from_a_text_leaf() {
    for key in [
        "VNOTE__REALTIME__COALESCE-REPLAY",
        "VNOTE__REALTIME__COALESCE_REPLAY",
    ] {
        let realtime: RealtimeConfig =
            load_group(&cfg(&[(key, "false")]), "realtime").expect("false should load");
        assert!(!realtime.coalesce_replay, "{key} should turn it off");
    }
}

/// The real leaf, as written to sovereign-config at
/// `/v-note/<env>/server/realtime/coalesce-replay`, is read from the
/// sovereign layer — in both settings.
#[test]
fn realtime_flag_is_read_from_the_sovereign_layer() {
    for (leaf, expected) in [("true", true), ("false", false)] {
        let config = cfg_with_sovereign(&[("realtime.coalesce-replay", leaf)], &[]);
        let realtime: RealtimeConfig =
            load_group(&config, "realtime").expect("sovereign value loads");

        assert_eq!(
            realtime.coalesce_replay, expected,
            "sovereign leaf `{leaf}`"
        );
    }
}

/// Env out-ranks sovereign-config, so an operator can force the rollback shape
/// on one container without touching the shared subtree.
#[test]
fn realtime_env_override_beats_the_sovereign_value() {
    let config = cfg_with_sovereign(
        &[("realtime.coalesce-replay", "true")],
        &[("VNOTE__REALTIME__COALESCE_REPLAY", "false")],
    );
    let realtime: RealtimeConfig = load_group(&config, "realtime").expect("env override loads");

    assert!(!realtime.coalesce_replay);
}

/// The sovereign layer is merged wholesale, so a `realtime` branch that exists
/// only for some later leaf must not silently drop the flag back to `false`.
#[test]
fn realtime_branch_without_the_flag_still_means_on() {
    let config = cfg_with_sovereign(&[("realtime.some-later-leaf", "x")], &[]);
    let realtime: RealtimeConfig =
        load_group(&config, "realtime").expect("a branch without the flag still loads");

    assert!(realtime.coalesce_replay);
}

/// A leaf that is not a boolean fails startup rather than being read as `false`
/// — a typo'd flag must not silently roll the change back.
#[test]
fn realtime_non_boolean_flag_fails_to_deserialize() {
    let config = cfg(&[("VNOTE__REALTIME__COALESCE-REPLAY", "yes-please")]);
    let error =
        load_group::<RealtimeConfig>(&config, "realtime").expect_err("non-boolean is rejected");

    match error {
        ConfigError::Load { ref group, .. } => assert_eq!(group, "realtime"),
        other => panic!("expected Load, got: {other}"),
    }
}

/// The mirror image of `coalesce-replay` (#342): compression ships **off**, so
/// the stack swap reaches a deployed environment inert and an operator has to
/// ask for it.
#[test]
fn realtime_compression_defaults_to_off() {
    let realtime: RealtimeConfig =
        load_group(&cfg(&[]), "realtime").expect("realtime group defaults");

    assert!(!realtime.compression);
}

/// Every leaf is text in sovereign-config, so the switch that turns #342 on in
/// dev has to work written as a string. Unlike `coalesce-replay` the key is a
/// single word, so there is no kebab/underscore pair to cover.
#[test]
fn realtime_compression_can_be_turned_on_from_a_text_leaf() {
    let realtime: RealtimeConfig = load_group(
        &cfg(&[("VNOTE__REALTIME__COMPRESSION", "true")]),
        "realtime",
    )
    .expect("true should load");

    assert!(realtime.compression);
}

/// The real leaf at `/v-note/<env>/server/realtime/compression`, read from
/// the sovereign layer in both settings.
#[test]
fn realtime_compression_is_read_from_the_sovereign_layer() {
    for (leaf, expected) in [("true", true), ("false", false)] {
        let config = cfg_with_sovereign(&[("realtime.compression", leaf)], &[]);
        let realtime: RealtimeConfig =
            load_group(&config, "realtime").expect("sovereign value loads");

        assert_eq!(realtime.compression, expected, "sovereign leaf `{leaf}`");
    }
}

/// Env out-ranks sovereign-config, so one container can be rolled back off
/// compression without touching the shared subtree — the stack-swap escape
/// hatch #342 exists for.
#[test]
fn realtime_compression_env_override_beats_the_sovereign_value() {
    let config = cfg_with_sovereign(
        &[("realtime.compression", "true")],
        &[("VNOTE__REALTIME__COMPRESSION", "false")],
    );
    let realtime: RealtimeConfig = load_group(&config, "realtime").expect("env override loads");

    assert!(!realtime.compression);
}

/// Both realtime flags are independent: a subtree carrying only one must not
/// disturb the other's default.
#[test]
fn realtime_flags_do_not_default_each_other() {
    let compression_only = cfg_with_sovereign(&[("realtime.compression", "true")], &[]);
    let realtime: RealtimeConfig =
        load_group(&compression_only, "realtime").expect("compression-only branch loads");
    assert!(realtime.compression);
    assert!(
        realtime.coalesce_replay,
        "compression must not disturb coalesce-replay's default"
    );

    let coalesce_only = cfg_with_sovereign(&[("realtime.coalesce-replay", "false")], &[]);
    let realtime: RealtimeConfig =
        load_group(&coalesce_only, "realtime").expect("coalesce-only branch loads");
    assert!(!realtime.coalesce_replay);
    assert!(
        !realtime.compression,
        "coalesce-replay must not disturb compression's default"
    );
}

/// A typo'd compression leaf fails startup rather than being read as `false` —
/// an operator who asked for compression must not get it silently ignored.
///
/// Note what this does *not* claim: the permissive spellings are accepted, so
/// `on` / `yes` / `1` all deserialize to `true` rather than failing. Only a
/// value that is not a boolean in any spelling is rejected.
#[test]
fn realtime_non_boolean_compression_fails_to_deserialize() {
    let config = cfg(&[("VNOTE__REALTIME__COMPRESSION", "yes-please")]);
    let error =
        load_group::<RealtimeConfig>(&config, "realtime").expect_err("non-boolean is rejected");

    match error {
        ConfigError::Load { ref group, .. } => assert_eq!(group, "realtime"),
        other => panic!("expected Load, got: {other}"),
    }
}

/// The permissive spellings above, pinned — so the claim in
/// `realtime_non_boolean_compression_fails_to_deserialize` stays honest if the
/// `config` crate's coercion ever changes.
#[test]
fn realtime_compression_accepts_the_permissive_boolean_spellings() {
    for leaf in ["on", "yes", "1", "TRUE"] {
        let realtime: RealtimeConfig =
            load_group(&cfg(&[("VNOTE__REALTIME__COMPRESSION", leaf)]), "realtime")
                .unwrap_or_else(|error| panic!("`{leaf}` should coerce: {error}"));
        assert!(realtime.compression, "`{leaf}` should mean on");
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
// whitespace trimming
// ---------------------------------------------------------------------------

/// The motivating case: JWT validation splits the token's `scope` claim on
/// whitespace and compares whole values, so a `required-scope` with a stray space
/// can never match — every user gets 403 while startup, the deploy health gate and
/// CI all report green. Trimming at the source makes that unrepresentable.
#[test]
fn required_scope_with_surrounding_whitespace_is_trimmed() {
    let config = cfg(&with(
        oidc_env(),
        &[("VNOTE__OIDC__REQUIRED-SCOPE", "  v-note:dev:access\n")],
    ));
    let oidc: OidcConfig = load_group(&config, "oidc").expect("should load");

    assert_eq!(oidc.required_scope, "v-note:dev:access");
    // The comparison auth.rs performs must now succeed.
    assert!(
        "openid profile v-note:dev:access"
            .split_whitespace()
            .any(|value| value == oidc.required_scope)
    );
}

/// Trimming is applied to every string leaf, not a curated list — including
/// secrets, where surrounding whitespace is a paste artefact rather than intent.
#[test]
fn every_string_leaf_is_trimmed_including_secrets() {
    let config = cfg(&[
        ("VNOTE__DATABASE__HOST", "  postgres  "),
        ("VNOTE__DATABASE__NAME", "\tv_note\n"),
        ("VNOTE__DATABASE__USER", " v_note "),
        ("VNOTE__DATABASE__PASSWORD", "  s3cret\n"),
        ("VNOTE__DATABASE__PORT", "  6543  "),
    ]);
    let database: DatabaseConfig = load_group(&config, "database").expect("should load");

    assert_eq!(database.host, "postgres");
    assert_eq!(database.name, "v_note");
    assert_eq!(database.user, "v_note");
    assert_eq!(database.password, "s3cret");
    // Trimming happens before coercion, so a padded number still parses.
    assert_eq!(database.port, 6543);
}

/// Trimming must not turn a whitespace-only value into a silently accepted one:
/// it becomes empty, which `validate` still rejects by field path.
#[test]
fn whitespace_only_value_is_still_rejected() {
    let config = cfg(&with(oidc_env(), &[("VNOTE__OIDC__CLIENT-ID", "   \n ")]));
    let error = load_group::<OidcConfig>(&config, "oidc").expect_err("blank client id rejected");

    match error {
        ConfigError::Invalid { ref path, .. } => assert_eq!(path, "oidc.client-id"),
        other => panic!("expected Invalid, got: {other}"),
    }
}

/// A trailing newline is the common real-world case — the dev `assetlinks-json`
/// leaf carried one from its original OpenBao value.
#[test]
fn trailing_newline_on_a_json_leaf_is_trimmed() {
    let json = r#"[{"relation":["delegate_permission/common.handle_all_urls"]}]"#;
    let padded = format!("{json}\n");
    let config = cfg(&[("VNOTE__ANDROID__ASSETLINKS-JSON", padded.as_str())]);
    let android: AndroidConfig = load_group(&config, "android").expect("should load");

    assert_eq!(android.assetlinks_json(), Some(json));
}

// ---------------------------------------------------------------------------
// shell-safe env names
// ---------------------------------------------------------------------------

/// Canonical leaf keys are kebab-case to match sovereign-config paths, but `-` is
/// not legal in a POSIX shell variable name — `VNOTE__OBSERVABILITY__METRICS-ADDR=x`
/// is parsed as a command, not an assignment. Every multi-word leaf therefore also
/// accepts the snake_case form via `#[serde(alias = …)]`.
///
/// This exercises **every** multi-word leaf, so a new field added without an alias
/// fails here rather than silently breaking `cargo run` for local dev.
#[test]
fn every_multi_word_leaf_accepts_a_shell_safe_snake_case_name() {
    // oidc
    let oidc: OidcConfig = load_group(
        &cfg(&[
            ("VNOTE__OIDC__ISSUER_URL", "https://auth.example/o/v-note/"),
            (
                "VNOTE__OIDC__AUTHORIZE_URL",
                "https://auth.example/authorize",
            ),
            ("VNOTE__OIDC__CLIENT_ID", "browser"),
            (
                "VNOTE__OIDC__REDIRECT_URI",
                "https://v.example/auth/callback",
            ),
            ("VNOTE__OIDC__REQUIRED_SCOPE", "v-note:prod:access"),
            (
                "VNOTE__OIDC__END_SESSION_URL",
                "https://auth.example/logout",
            ),
        ]),
        "oidc",
    )
    .expect("snake_case oidc leaves should load");
    assert_eq!(oidc.client_id, "browser");
    assert_eq!(oidc.required_scope, "v-note:prod:access");
    assert!(oidc.authorize_url.is_some());
    assert!(oidc.end_session_url.is_some());

    // observability
    let observability: ObservabilityConfig = load_group(
        &cfg(&[
            ("VNOTE__OBSERVABILITY__ENVIRONMENT", "dev"),
            ("VNOTE__OBSERVABILITY__OTLP_ENDPOINT", "http://alloy:4317/"),
            ("VNOTE__OBSERVABILITY__OTLP_PROTOCOL", "grpc"),
            ("VNOTE__OBSERVABILITY__OTLP_TIMEOUT_MS", "7000"),
            ("VNOTE__OBSERVABILITY__SERVICE_NAME", "v-note-snake"),
            ("VNOTE__OBSERVABILITY__METRICS_ADDR", "127.0.0.1:9111"),
        ]),
        "observability",
    )
    .expect("snake_case observability leaves should load");
    assert_eq!(observability.otlp_timeout_ms, 7000);
    assert_eq!(observability.service_name, "v-note-snake");
    assert_eq!(
        observability.metrics_socket_addr(),
        Some("127.0.0.1:9111".parse().expect("addr"))
    );
    assert!(observability.otlp_endpoint.is_some());

    // android
    let android: AndroidConfig = load_group(
        &cfg(&[("VNOTE__ANDROID__ASSETLINKS_JSON", "[]")]),
        "android",
    )
    .expect("snake_case android leaf should load");
    assert_eq!(android.assetlinks_json(), Some("[]"));

    // server
    let server: ServerConfig = load_group(
        &cfg(&[
            ("VNOTE__SERVER__HTTP_PORT", "9001"),
            ("VNOTE__SERVER__TLS_CERT", "/app/cert.pem"),
            ("VNOTE__SERVER__TLS_KEY", "/app/key.pem"),
            ("VNOTE__SERVER__STATIC_DIR", "/app/dist"),
        ]),
        "server",
    )
    .expect("snake_case server leaves should load");
    assert_eq!(server.http_port, 9001);
    assert!(server.tls_pair().is_some());
    assert!(server.static_dir.is_some());
}

/// Both spellings must reach the same field — kebab for sovereign-config/compose
/// parity, snake for shell assignment.
#[test]
fn kebab_and_snake_names_are_interchangeable() {
    let kebab: ServerConfig =
        load_group(&cfg(&[("VNOTE__SERVER__HTTP-PORT", "9100")]), "server").expect("kebab loads");
    let snake: ServerConfig =
        load_group(&cfg(&[("VNOTE__SERVER__HTTP_PORT", "9100")]), "server").expect("snake loads");

    assert_eq!(kebab.http_port, snake.http_port);
    assert_eq!(kebab.http_port, 9100);
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

    // The `oidc` group no longer holds a secret (#274 made the client public),
    // so `database/password` is the only value this guarantee has to cover.
}

/// The redaction guarantee that *is* unconditional: checks I perform myself never
/// echo the value, only the field path.
#[test]
fn validate_errors_name_the_field_but_never_the_value() {
    let config = cfg(&with(oidc_env(), &[("VNOTE__OIDC__REQUIRED-SCOPE", "   ")]));
    let error = load_group::<OidcConfig>(&config, "oidc").expect_err("blank scope rejected");
    assert!(error.to_string().contains("oidc.required-scope"));
    assert!(!error.to_string().contains("   "));

    // A blank client id is reported by path alone.
    let config = cfg(&with(oidc_env(), &[("VNOTE__OIDC__CLIENT-ID", " ")]));
    let error = load_group::<OidcConfig>(&config, "oidc").expect_err("blank client id rejected");
    assert_eq!(
        error.to_string(),
        "invalid config `oidc.client-id`: must not be empty"
    );
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

/// Deploys used to inject `VNOTE__OBSERVABILITY__METRICS_ADDR` so the deploy script
/// and the app agreed on the listener address. Iteration 22 removed that override:
/// the script now reads the same `observability/metrics-addr` leaf through the
/// Woodpecker broker, and the app reads it from its own sovereign layer. That only
/// works if a sovereign leaf out-ranks the built-in default with no env var
/// present — which nothing asserted before.
#[test]
fn sovereign_layer_supplies_metrics_addr_with_no_env_override() {
    let env = [("VNOTE__OBSERVABILITY__ENVIRONMENT", "dev")];

    let defaults_only: ObservabilityConfig =
        load_group(&cfg(&env), "observability").expect("loads");
    let with_sovereign: ObservabilityConfig = load_group(
        &cfg_with_sovereign(&[("observability.metrics-addr", "0.0.0.0:9191")], &env),
        "observability",
    )
    .expect("loads");

    assert_eq!(
        with_sovereign.metrics_socket_addr(),
        Some("0.0.0.0:9191".parse().expect("addr")),
        "the sovereign leaf must beat the built-in default"
    );
    assert_ne!(
        defaults_only.metrics_socket_addr(),
        with_sovereign.metrics_socket_addr(),
        "the fixture must differ from the default, or this proves nothing"
    );
}

/// The env layer is still the per-deploy escape hatch above sovereign-config —
/// removing the deploy's use of it must not quietly remove the capability.
#[test]
fn env_layer_still_overrides_the_sovereign_metrics_addr() {
    let config = cfg_with_sovereign(
        &[("observability.metrics-addr", "0.0.0.0:9191")],
        &[
            ("VNOTE__OBSERVABILITY__ENVIRONMENT", "dev"),
            ("VNOTE__OBSERVABILITY__METRICS_ADDR", "127.0.0.1:9292"),
        ],
    );
    let observability: ObservabilityConfig = load_group(&config, "observability").expect("loads");

    assert_eq!(
        observability.metrics_socket_addr(),
        Some("127.0.0.1:9292".parse().expect("addr"))
    );
}

// ---------------------------------------------------------------------------
// client-telemetry (#354)
// ---------------------------------------------------------------------------

const SPA_ENDPOINT: (&str, &str) = ("VNOTE__CLIENT_TELEMETRY__SPA_ENDPOINT", "http://alloy:4318");
const ANDROID_ENDPOINT: (&str, &str) = (
    "VNOTE__CLIENT_TELEMETRY__ANDROID_ENDPOINT",
    "http://alloy:4319",
);
const ENABLED: (&str, &str) = ("VNOTE__CLIENT_TELEMETRY__ENABLED", "true");

/// The kill switch ships off, and "off" has to be reachable with no
/// `client-telemetry` leaf anywhere — that is every environment on the day this
/// lands, before anyone has written one.
#[test]
fn client_telemetry_is_off_with_no_leaves_at_all() {
    let telemetry: ClientTelemetryConfig =
        load_group(&cfg(&[]), "client-telemetry").expect("group defaults");

    assert!(!telemetry.enabled);
    assert_eq!(telemetry.upstreams(), None);
}

/// Both spellings of the group segment: `CLIENT-TELEMETRY` is the canonical
/// kebab form compose files use, `CLIENT_TELEMETRY` is the only one a POSIX
/// shell can export. The group name is the first in this file to contain a
/// hyphen, so the snake-to-kebab fold is being relied on for a *group* here,
/// not just for a leaf as everywhere else.
#[test]
fn client_telemetry_group_name_is_addressable_in_both_spellings() {
    for prefix in ["VNOTE__CLIENT-TELEMETRY__", "VNOTE__CLIENT_TELEMETRY__"] {
        let enabled = format!("{prefix}ENABLED");
        let spa = format!("{prefix}SPA-ENDPOINT");
        let android = format!("{prefix}ANDROID_ENDPOINT");
        let telemetry: ClientTelemetryConfig = load_group(
            &cfg(&[
                (enabled.as_str(), "true"),
                (spa.as_str(), "http://alloy:4318"),
                (android.as_str(), "http://alloy:4319"),
            ]),
            "client-telemetry",
        )
        .unwrap_or_else(|error| panic!("{prefix} should load: {error}"));

        let upstreams = telemetry.upstreams().expect("enabled with both endpoints");
        assert_eq!(upstreams.spa.as_str(), "http://alloy:4318/");
        assert_eq!(upstreams.android.as_str(), "http://alloy:4319/");
    }
}

/// Endpoints alone do not turn ingest on. The switch is the switch.
#[test]
fn client_telemetry_endpoints_without_the_switch_stay_off() {
    let telemetry: ClientTelemetryConfig =
        load_group(&cfg(&[SPA_ENDPOINT, ANDROID_ENDPOINT]), "client-telemetry").expect("loads");

    assert_eq!(telemetry.upstreams(), None);
}

/// "Enabled with nowhere to send" must fail at startup, naming the leaf —
/// otherwise it surfaces as a 502 on every export, which looks like a sidecar
/// outage rather than a missing value.
#[test]
fn client_telemetry_enabled_requires_both_endpoints() {
    for (present, missing) in [
        (SPA_ENDPOINT, "client-telemetry.android-endpoint"),
        (ANDROID_ENDPOINT, "client-telemetry.spa-endpoint"),
    ] {
        let error =
            load_group::<ClientTelemetryConfig>(&cfg(&[ENABLED, present]), "client-telemetry")
                .expect_err("a missing endpoint should be rejected");

        let message = error.to_string();
        assert!(
            message.contains(missing),
            "should name {missing}: {message}"
        );
        assert!(message.contains("required when"), "{message}");
    }
}

/// A blank leaf is how deploy tooling renders "unset", so it must read as
/// absent — and therefore as missing when the switch is on.
#[test]
fn client_telemetry_blank_endpoint_counts_as_missing() {
    let error = load_group::<ClientTelemetryConfig>(
        &cfg(&[
            ENABLED,
            SPA_ENDPOINT,
            ("VNOTE__CLIENT_TELEMETRY__ANDROID_ENDPOINT", "   "),
        ]),
        "client-telemetry",
    )
    .expect_err("a blank endpoint should be rejected when enabled");

    assert!(
        error
            .to_string()
            .contains("client-telemetry.android-endpoint"),
        "{error}"
    );
}

/// `Url::join` replaces the last segment of a base with no trailing slash, so
/// `http://alloy:4318/otlp` + `v1/traces` silently becomes `/v1/traces`. Only a
/// bare origin is accepted, which is the one shape that cannot do that.
#[test]
fn client_telemetry_endpoint_must_be_a_bare_http_origin() {
    for (value, expected) in [
        ("http://alloy:4318/otlp", "bare origin"),
        ("http://alloy:4318/otlp/", "bare origin"),
        ("http://alloy:4318/?x=1", "bare origin"),
        ("ftp://alloy:4318", "http(s)"),
    ] {
        let error = load_group::<ClientTelemetryConfig>(
            &cfg(&[
                ENABLED,
                ("VNOTE__CLIENT_TELEMETRY__SPA_ENDPOINT", value),
                ANDROID_ENDPOINT,
            ]),
            "client-telemetry",
        )
        .expect_err("a non-origin endpoint should be rejected");

        let message = error.to_string();
        assert!(
            message.contains("client-telemetry.spa-endpoint"),
            "{value}: {message}"
        );
        assert!(message.contains(expected), "{value}: {message}");
    }
}

/// A malformed endpoint behind a *disabled* switch still fails startup. Found
/// now, it is a typo; found when someone flips the switch during an incident,
/// it is the incident.
#[test]
fn client_telemetry_bad_endpoint_is_rejected_even_when_disabled() {
    let error = load_group::<ClientTelemetryConfig>(
        &cfg(&[(
            "VNOTE__CLIENT_TELEMETRY__SPA_ENDPOINT",
            "http://alloy:4318/otlp",
        )]),
        "client-telemetry",
    )
    .expect_err("validated regardless of the switch");

    assert!(error.to_string().contains("bare origin"), "{error}");
}

/// The switch is a text leaf in sovereign-config and has to work from there,
/// with env still able to override it on one container.
#[test]
fn client_telemetry_switch_is_read_from_sovereign_and_env_overrides_it() {
    let sovereign = [
        ("client-telemetry.enabled", "true"),
        ("client-telemetry.spa-endpoint", "http://alloy:4318"),
        ("client-telemetry.android-endpoint", "http://alloy:4319"),
    ];

    let on: ClientTelemetryConfig =
        load_group(&cfg_with_sovereign(&sovereign, &[]), "client-telemetry").expect("loads");
    assert!(on.upstreams().is_some(), "sovereign leaf should turn it on");

    let forced_off: ClientTelemetryConfig = load_group(
        &cfg_with_sovereign(&sovereign, &[("VNOTE__CLIENT_TELEMETRY__ENABLED", "false")]),
        "client-telemetry",
    )
    .expect("loads");
    assert_eq!(
        forced_off.upstreams(),
        None,
        "env should be able to kill it"
    );
}
