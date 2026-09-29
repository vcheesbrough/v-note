//! The SPA's `Content-Security-Policy` (#444).
//!
//! Since #439 the SPA holds an OIDC access token in JavaScript (the
//! `GET /api/telemetry/config` handover), so script injection in the page can
//! read a credential rather than only cause one to be sent. The policy bounds
//! that: only the app's own origin may supply script, and the page may only
//! talk to the app and to this environment's telemetry ingest.
//!
//! It is served by the app, not by the proxy in front of it, so it versions
//! with the SPA it describes: Trunk writes the wasm bootstrap as an inline
//! `<script type="module">` whose text names content-hashed file names, so its
//! hash changes with every build. The server reads the `index.html` it is about
//! to serve once, at startup, and allows exactly the inline scripts in it by
//! hash — no `'unsafe-inline'` for script, and no per-request nonce rewriting.

use axum::http::HeaderValue;
use base64::Engine as _;
use sha2::{Digest, Sha256};

/// The policy for the SPA, given the hash sources of `index.html`'s inline
/// scripts (see [`inline_script_hashes`]) and the extra origins the page may
/// `fetch` besides its own.
///
/// - `script-src 'self' 'wasm-unsafe-eval'` plus the bootstrap's hash: the
///   module and its wasm come from the app; `'wasm-unsafe-eval'` compiles the
///   wasm without granting JavaScript `eval`.
/// - `connect-src 'self'` covers the REST API, both realtime WebSockets (CSP
///   Level 3 `'self'` matches `wss:` on the same host) and, in a deployed
///   environment, the telemetry ingest Traefik routes on the app's own host.
///   The ingest origin is still listed when configured, because nothing makes
///   it same-origin — the e2e stack and `just run-compose` serve it on its own
///   port.
/// - `style-src 'unsafe-inline'`: styles cannot read the token, and the
///   stylesheet plus any inline style Leptos sets stay working.
/// - `frame-ancestors 'self'` carries over what the shared `security-headers`
///   middleware used to set, since the app's policy now replaces it.
pub fn spa_policy(script_hashes: &[String], connect_origins: &[String]) -> String {
    let mut script_src = String::from("script-src 'self' 'wasm-unsafe-eval'");
    for hash in script_hashes {
        script_src.push(' ');
        script_src.push_str(hash);
    }
    let mut connect_src = String::from("connect-src 'self'");
    for origin in connect_origins {
        connect_src.push(' ');
        connect_src.push_str(origin);
    }
    [
        "default-src 'self'",
        &script_src,
        &connect_src,
        "img-src 'self' data: blob:",
        "style-src 'self' 'unsafe-inline'",
        "frame-ancestors 'self'",
        "base-uri 'none'",
        "object-src 'none'",
    ]
    .join("; ")
}

/// The policy as a header value. Every input is either a fixed string, a
/// base64 hash, or a URL origin serialisation, none of which can hold a byte a
/// header value refuses — so failing here is a bug, not bad config.
pub fn spa_policy_header(script_hashes: &[String], connect_origins: &[String]) -> HeaderValue {
    HeaderValue::from_str(&spa_policy(script_hashes, connect_origins))
        .expect("CSP is built from header-safe parts")
}

/// `'sha256-…'` source expressions for every inline `<script>` in `html`, in
/// document order — the hash of the exact text between the opening tag and
/// `</script>`, which is what a browser hashes. Scripts with a `src` attribute
/// are loaded by URL and covered by `'self'`, so they are skipped.
///
/// A scanner, not an HTML parser: it only has to read what Trunk writes, and
/// the e2e suite fails on any CSP violation, so a script it misses shows up as
/// a refused bootstrap there rather than in production.
pub fn inline_script_hashes(html: &str) -> Vec<String> {
    let lower = html.to_ascii_lowercase();
    let mut hashes = Vec::new();
    let mut cursor = 0;
    while let Some(found) = lower[cursor..].find("<script") {
        let tag_start = cursor + found;
        let after_name = tag_start + "<script".len();
        // `<scripts>` or `<script-x>` is not a script element.
        match lower[after_name..].chars().next() {
            Some(c) if c == '>' || c == '/' || c.is_ascii_whitespace() => {}
            _ => {
                cursor = after_name;
                continue;
            }
        }
        let Some(tag_len) = lower[after_name..].find('>') else {
            break;
        };
        let attributes = &lower[after_name..after_name + tag_len];
        let body_start = after_name + tag_len + 1;
        let Some(body_len) = lower[body_start..].find("</script") else {
            break;
        };
        let body_end = body_start + body_len;
        if !has_src_attribute(attributes) {
            hashes.push(sha256_source(&html[body_start..body_end]));
        }
        cursor = body_end;
    }
    hashes
}

fn has_src_attribute(attributes: &str) -> bool {
    attributes
        .split(|c: char| c.is_ascii_whitespace())
        .any(|attribute| attribute == "src" || attribute.starts_with("src="))
}

fn sha256_source(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    format!(
        "'sha256-{}'",
        base64::engine::general_purpose::STANDARD.encode(digest)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `trunk build --release` (0.21.14) writes for `frontend/index.html`,
    /// abridged to the parts that matter: a hashed stylesheet, the inline
    /// bootstrap module, and the preload links.
    const TRUNK_INDEX: &str = r#"<!doctype html>
<html lang="en">
  <head>
    <link rel="stylesheet" href="/styles-9bb438f9d3dad905.css" integrity="sha384-x"/>
<script type="module">
import init, * as bindings from '/frontend-4870a71f952291fa.js';
const wasm = await init({ module_or_path: '/frontend-4870a71f952291fa_bg.wasm' });


window.wasmBindings = bindings;


dispatchEvent(new CustomEvent("TrunkApplicationStarted", {detail: {wasm}}));

</script>
  <link rel="modulepreload" href="/frontend-4870a71f952291fa.js" crossorigin="anonymous"></head>
  <body></body>
</html>"#;

    fn bootstrap_text() -> &'static str {
        let start = TRUNK_INDEX.find("<script type=\"module\">").unwrap() + 22;
        let end = TRUNK_INDEX.find("</script>").unwrap();
        &TRUNK_INDEX[start..end]
    }

    #[test]
    fn trunk_bootstrap_is_hashed_byte_for_byte() {
        let hashes = inline_script_hashes(TRUNK_INDEX);
        assert_eq!(hashes, vec![sha256_source(bootstrap_text())]);
        assert!(hashes[0].starts_with("'sha256-") && hashes[0].ends_with("='"));
    }

    /// A known vector, so the encoding (standard base64 with padding, over the
    /// raw digest) is pinned against what browsers expect, not against itself.
    #[test]
    fn hash_source_matches_the_csp_encoding() {
        assert_eq!(
            sha256_source("alert(1)"),
            "'sha256-bhHHL3z2vDgxUt0W3dWQOrprscmda2Y5pLsLg4GF+pI='"
        );
    }

    #[test]
    fn external_and_non_script_tags_are_not_hashed() {
        let html = r#"<script src="/a.js"></script><scripts>x</scripts>
<SCRIPT type="module" SRC='/b.js'></SCRIPT><script>one()</script><script
>two()</Script>"#;
        assert_eq!(
            inline_script_hashes(html),
            vec![sha256_source("one()"), sha256_source("two()")]
        );
    }

    #[test]
    fn a_page_without_inline_script_has_no_hashes() {
        assert!(inline_script_hashes("<html><body></body></html>").is_empty());
        assert!(inline_script_hashes("<script>unterminated").is_empty());
    }

    #[test]
    fn policy_lists_hashes_and_extra_connect_origins() {
        let policy = spa_policy(
            &["'sha256-abc='".to_string()],
            &["https://otlp-collector-oidc:4318".to_string()],
        );
        assert_eq!(
            policy,
            "default-src 'self'; \
             script-src 'self' 'wasm-unsafe-eval' 'sha256-abc='; \
             connect-src 'self' https://otlp-collector-oidc:4318; \
             img-src 'self' data: blob:; \
             style-src 'self' 'unsafe-inline'; \
             frame-ancestors 'self'; \
             base-uri 'none'; \
             object-src 'none'"
        );
    }

    #[test]
    fn policy_without_extras_is_same_origin_only() {
        let policy = spa_policy(&[], &[]);
        assert!(policy.contains("script-src 'self' 'wasm-unsafe-eval';"));
        assert!(policy.contains("connect-src 'self';"));
        // JavaScript eval stays off; only wasm compilation is allowed.
        assert!(!policy.contains(" 'unsafe-eval'"));
        assert_eq!(spa_policy_header(&[], &[]).to_str().expect("ascii"), policy);
    }
}
