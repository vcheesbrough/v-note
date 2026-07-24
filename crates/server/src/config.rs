//! Runtime configuration loaded from layered sources.
//!
//! Config is assembled from three layers (lowest priority first):
//!   1. in-memory defaults (ports, otlp protocol/timeout, service name, metrics addr);
//!   2. the managed [`sovereign-config`] subtree — added **only** when the access
//!      URL is present (`SOVEREIGN_CONFIG_ACCESS_URL_FILE` / `SOVEREIGN_CONFIG_ACCESS_URL`),
//!      so local dev and e2e (which have no sovereign-config server) fall back to plain env;
//!   3. environment overrides under the `VNOTE__` prefix with a `__` nesting separator.
//!
//! Each top-level group is its own self-contained DTO deserialized from its own
//! sub-branch (`database`, `oidc`, `observability`, `android`) — there is no
//! umbrella config struct. Every leaf in sovereign-config is text; rich field
//! types (`u16`, `SocketAddr`, `url::Url`, …) fold presence + coercion checks into
//! deserialization, and each DTO additionally implements [`ValidatedConfig`] for the
//! residual checks the type system can't express. All groups are loaded through the
//! single [`load_group`] choke point, which validates and redacts uniformly.

use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use config::{Config, ConfigBuilder, Environment};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use sovereign_config_provider::SovereignConfigSource;
use url::Url;

/// Env vars whose presence enables the sovereign-config source layer.
const SOVEREIGN_ACCESS_URL_FILE: &str = "SOVEREIGN_CONFIG_ACCESS_URL_FILE";
const SOVEREIGN_ACCESS_URL: &str = "SOVEREIGN_CONFIG_ACCESS_URL";

/// Prefix + separator for the environment override layer.
///
/// `VNOTE__OIDC__CLIENT-ID=…` maps to the `oidc.client-id` leaf. Multi-word leaf
/// segments are kebab-case to match sovereign-config path segments (which forbid
/// `_`), so the same key overrides across every layer.
const ENV_PREFIX: &str = "VNOTE";
const ENV_SEPARATOR: &str = "__";

/// A configuration failure, carrying enough to locate the offending group/field
/// but **never** the offending value (secret leaves stay redacted).
pub enum ConfigError {
    /// The layered `config::Config` could not be assembled (e.g. sovereign-config
    /// connection/reveal failed, or a default was rejected).
    Build(config::ConfigError),
    /// A group could not be deserialized from its sub-branch (missing field or a
    /// leaf that would not coerce to the target type).
    ///
    /// **Invariant:** `source` is `config`'s own message, which embeds the
    /// offending value on a type mismatch (`invalid type: string "…"`). Secret
    /// leaves must therefore stay `String`-typed — `String` cannot fail coercion,
    /// so a secret can never reach this branch and never appears in a startup log.
    /// Pinned by `secret_leaves_never_leak_their_value_in_errors`.
    Load {
        group: String,
        source: config::ConfigError,
    },
    /// A group deserialized but failed a [`ValidatedConfig::validate`] check.
    Invalid { path: String, reason: String },
}

impl ConfigError {
    /// Build an [`Invalid`](ConfigError::Invalid) error. `path` should name the
    /// field/leaf; `reason` must not embed any secret value.
    pub fn invalid(path: impl Into<String>, reason: impl Into<String>) -> Self {
        ConfigError::Invalid {
            path: path.into(),
            reason: reason.into(),
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Build(source) => write!(f, "failed to build configuration: {source}"),
            ConfigError::Load { group, source } => {
                write!(f, "failed to load config group `{group}`: {source}")
            }
            ConfigError::Invalid { path, reason } => {
                write!(f, "invalid config `{path}`: {reason}")
            }
        }
    }
}

/// Startup failures surface through `Termination`, which prints `Debug`. Delegate to
/// `Display` so an operator sees the redacted one-line reason, not a struct dump.
impl fmt::Debug for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::Build(source) | ConfigError::Load { source, .. } => Some(source),
            ConfigError::Invalid { .. } => None,
        }
    }
}

/// A config group DTO that can validate the residue its field types can't express.
///
/// The body is intentionally allowed to be empty when rich field types already
/// make illegal states unrepresentable.
pub trait ValidatedConfig: DeserializeOwned {
    fn validate(&self) -> Result<(), ConfigError>;
}

/// Returns whether the sovereign-config access URL is present in the environment.
///
/// When absent (local dev / e2e / unit tests) the sovereign source is skipped and
/// config comes from defaults + env only.
pub fn sovereign_source_enabled() -> bool {
    [SOVEREIGN_ACCESS_URL_FILE, SOVEREIGN_ACCESS_URL]
        .iter()
        .any(|name| std::env::var(name).is_ok_and(|value| !value.trim().is_empty()))
}

/// Assemble the layered `config::Config`.
///
/// This **blocks** while the sovereign-config source connects and reveals secrets
/// (on its own dedicated I/O thread), so call it once at startup — from
/// `spawn_blocking` when on an async runtime.
pub fn build_config() -> Result<Config, ConfigError> {
    let mut builder = Config::builder();
    builder = apply_defaults(builder)?;

    if sovereign_source_enabled() {
        builder = builder.add_source(SovereignConfigSource::initialise_from_default_environment());
    }

    builder = builder.add_source(env_source());

    builder.build().map_err(ConfigError::Build)
}

/// The environment override layer. `VNOTE__OIDC__CLIENT-ID` → `oidc.client-id`.
fn env_source() -> Environment {
    Environment::with_prefix(ENV_PREFIX)
        .prefix_separator(ENV_SEPARATOR)
        .separator(ENV_SEPARATOR)
}

fn apply_defaults(
    builder: ConfigBuilder<config::builder::DefaultState>,
) -> Result<ConfigBuilder<config::builder::DefaultState>, ConfigError> {
    // Every leaf is text; typed coercion happens at group deserialization.
    let defaults = [
        ("database.port", "5432"),
        ("observability.otlp-protocol", "grpc"),
        ("observability.otlp-timeout-ms", "2000"),
        ("observability.service-name", "v-note"),
        ("observability.metrics-addr", "0.0.0.0:9090"),
        // Plain-HTTP fallback port; TLS (when configured) always binds :443.
        ("server.http-port", "8080"),
        // Present so the `android` sub-branch always exists; empty means "not configured".
        ("android.assetlinks-json", ""),
    ];
    let mut builder = builder;
    for (key, value) in defaults {
        builder = builder
            .set_default(key, value)
            .map_err(ConfigError::Build)?;
    }
    Ok(builder)
}

/// The single choke point: deserialize a group from its sub-branch (presence +
/// coercion) then validate it, wrapping errors with the group path.
pub fn load_group<T: ValidatedConfig>(cfg: &Config, group: &str) -> Result<T, ConfigError> {
    let dto: T = cfg.get(group).map_err(|source| ConfigError::Load {
        group: group.to_string(),
        source,
    })?;
    dto.validate()?;
    Ok(dto)
}

/// Deserialize an optional leaf, treating a blank value as absent.
///
/// Deployment tooling routinely renders an unset value as an empty string, and an
/// empty string is not a valid `Url`. Collapsing blank → `None` keeps that from
/// being a hard startup failure and preserves the pre-migration env semantics.
fn blank_as_none<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: std::str::FromStr,
    T::Err: fmt::Display,
{
    match Option::<String>::deserialize(deserializer)? {
        Some(raw) if !raw.trim().is_empty() => raw
            .trim()
            .parse()
            .map(Some)
            .map_err(serde::de::Error::custom),
        _ => Ok(None),
    }
}

/// Reject a secret/scalar that is empty after trimming, naming the field (never the value).
fn require_non_empty(path: &str, value: &str) -> Result<(), ConfigError> {
    if value.trim().is_empty() {
        return Err(ConfigError::invalid(path, "must not be empty"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// database
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct DatabaseConfig {
    pub host: String,
    pub port: u16,
    pub name: String,
    pub user: String,
    pub password: String,
}

impl ValidatedConfig for DatabaseConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        require_non_empty("database.host", &self.host)?;
        require_non_empty("database.name", &self.name)?;
        require_non_empty("database.user", &self.user)?;
        require_non_empty("database.password", &self.password)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// oidc
// ---------------------------------------------------------------------------

/// Optional, non-secret native-app OIDC leaves. Present → a second accepted JWT
/// `aud`/`iss` for the Android app (see #274).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct OidcAndroidConfig {
    #[serde(default, deserialize_with = "blank_as_none")]
    pub client_id: Option<String>,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub issuer_url: Option<Url>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct OidcConfig {
    pub issuer_url: Url,
    /// Overrides the authorization endpoint from OIDC discovery when set.
    #[serde(default, deserialize_with = "blank_as_none")]
    pub authorize_url: Option<Url>,
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uri: Url,
    pub required_scope: String,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub end_session_url: Option<Url>,
    #[serde(default)]
    pub android: OidcAndroidConfig,
}

impl ValidatedConfig for OidcConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        require_non_empty("oidc.client-id", &self.client_id)?;
        require_non_empty("oidc.client-secret", &self.client_secret)?;
        require_non_empty("oidc.required-scope", &self.required_scope)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// observability
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ObservabilityConfig {
    /// OTEL `deployment.environment` attribute (e.g. `dev` | `production`).
    pub environment: String,
    /// OTLP exporter endpoint; tracing export is disabled when absent or blank.
    #[serde(default, deserialize_with = "blank_as_none")]
    pub otlp_endpoint: Option<Url>,
    pub otlp_protocol: String,
    pub otlp_timeout_ms: u64,
    pub service_name: String,
    /// `host:port`, or `disabled`/empty to turn the metrics listener off.
    pub metrics_addr: String,
}

impl ObservabilityConfig {
    /// The resolved metrics listener address, or `None` when disabled.
    pub fn metrics_socket_addr(&self) -> Option<SocketAddr> {
        if metrics_disabled(&self.metrics_addr) {
            None
        } else {
            // validate() guarantees this parses.
            self.metrics_addr.parse().ok()
        }
    }

    pub fn otlp_timeout(&self) -> Duration {
        Duration::from_millis(self.otlp_timeout_ms)
    }
}

fn metrics_disabled(value: &str) -> bool {
    value.trim().is_empty() || value.eq_ignore_ascii_case("disabled")
}

impl ValidatedConfig for ObservabilityConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        require_non_empty("observability.environment", &self.environment)?;
        if self.otlp_protocol != "grpc" {
            return Err(ConfigError::invalid(
                "observability.otlp-protocol",
                format!(
                    "unsupported protocol `{}`; expected `grpc`",
                    self.otlp_protocol
                ),
            ));
        }
        if !metrics_disabled(&self.metrics_addr) && self.metrics_addr.parse::<SocketAddr>().is_err()
        {
            return Err(ConfigError::invalid(
                "observability.metrics-addr",
                "must be `host:port` or `disabled`",
            ));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// android
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct AndroidConfig {
    /// Raw Digital Asset Links JSON served at `/.well-known/assetlinks.json`.
    /// Empty means "not configured" (route returns 404).
    #[serde(default)]
    pub assetlinks_json: String,
}

impl AndroidConfig {
    /// The assetlinks JSON when configured, else `None`.
    pub fn assetlinks_json(&self) -> Option<&str> {
        let trimmed = self.assetlinks_json.trim();
        (!trimmed.is_empty()).then_some(self.assetlinks_json.as_str())
    }
}

impl ValidatedConfig for AndroidConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        // Single string leaf parsed in-app: reject a non-empty value that isn't valid JSON.
        if let Some(json) = self.assetlinks_json() {
            if serde_json::from_str::<serde_json::Value>(json).is_err() {
                return Err(ConfigError::invalid(
                    "android.assetlinks-json",
                    "must be valid JSON",
                ));
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// server
// ---------------------------------------------------------------------------

/// How the process exposes itself. These are image-internal (identical across
/// deployments), so they come from config defaults or `VNOTE__SERVER__*` overrides
/// set by the image — never from sovereign-config.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ServerConfig {
    /// Plain-HTTP listen port, used only when TLS is not configured.
    pub http_port: u16,
    /// PEM certificate path; when set together with `tls-key`, the server binds
    /// TLS on `:443` instead of plain HTTP.
    #[serde(default, deserialize_with = "blank_as_none")]
    pub tls_cert: Option<PathBuf>,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub tls_key: Option<PathBuf>,
    /// When set, the SPA is served from this directory with an `index.html` fallback.
    #[serde(default, deserialize_with = "blank_as_none")]
    pub static_dir: Option<PathBuf>,
}

impl ServerConfig {
    /// The cert/key pair when both are configured, else `None` (plain HTTP).
    ///
    /// `validate` guarantees the pair is all-or-nothing, so a lone value never
    /// reaches here.
    pub fn tls_pair(&self) -> Option<(&PathBuf, &PathBuf)> {
        self.tls_cert.as_ref().zip(self.tls_key.as_ref())
    }
}

impl ValidatedConfig for ServerConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        match (&self.tls_cert, &self.tls_key) {
            (Some(_), None) => Err(ConfigError::invalid(
                "server.tls-key",
                "required when `server.tls-cert` is set",
            )),
            (None, Some(_)) => Err(ConfigError::invalid(
                "server.tls-cert",
                "required when `server.tls-key` is set",
            )),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests;
