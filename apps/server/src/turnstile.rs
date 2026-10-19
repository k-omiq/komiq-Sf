//! Cloudflare Turnstile on the sensitive mutations (`KOMIKA_TURNSTILE=on`).
//!
//! Off by default: a self-hosted server without Cloudflare never asks for a token, and
//! every mutation behaves as before. On, sign-up, sign-in, password changes and resets,
//! comments, reviews and reports each carry a single-use token from the browser widget
//! (`turnstileToken`), and the resolver checks it with Cloudflare's `siteverify` after
//! its rate limit and input checks, before any costly or lasting work.
//!
//! The site key is public: `instance.turnstileSiteKey` hands it to the web apps, so a
//! reader build needs no key of its own. The secret never leaves the server.
//!
//! A check never holds a request for long: each siteverify call has a short timeout and
//! is retried once with the same idempotency key, so a retried call cannot spend the
//! token twice. When Cloudflare cannot be reached the mutation is refused, never let
//! through.

use std::time::Duration;

use crate::config::Secret;

/// Cloudflare's server-side check.
pub const SITEVERIFY_URL: &str = "https://challenges.cloudflare.com/turnstile/v0/siteverify";
/// Cloudflare's own limit on a token's length. A longer one cannot be valid.
const MAX_TOKEN_LEN: usize = 2048;
/// One siteverify call. Two attempts keep the worst case near five seconds.
const ATTEMPT_TIMEOUT: Duration = Duration::from_millis(2500);
const CONNECT_TIMEOUT: Duration = Duration::from_millis(1500);
const ATTEMPTS: usize = 2;

/// `KOMIKA_TURNSTILE`, `KOMIKA_TURNSTILE_SITE_KEY` and `KOMIKA_TURNSTILE_SECRET`.
#[derive(Clone, Debug)]
pub struct TurnstileConfig {
    /// The widget's public site key.
    pub site_key: String,
    /// The widget's secret key. `Debug` prints it redacted (`Config` is logged).
    pub secret: Secret,
}

impl TurnstileConfig {
    /// `None` when the switch is off or unset. On without a site key or a secret is an
    /// error, so a server never starts with a check it cannot run.
    pub fn parse(get: impl Fn(&str) -> Option<String>) -> Result<Option<Self>, String> {
        let switch = get("KOMIKA_TURNSTILE")
            .map(|v| v.trim().to_ascii_lowercase())
            .unwrap_or_default();
        match switch.as_str() {
            "" | "off" | "0" | "false" => return Ok(None),
            "on" | "1" | "true" => {}
            other => return Err(format!("KOMIKA_TURNSTILE must be on or off, not {other:?}")),
        }
        let required = |name: &str, what: &str| {
            get(name)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .ok_or_else(|| format!("KOMIKA_TURNSTILE=on needs {name} ({what})"))
        };
        Ok(Some(Self {
            site_key: required("KOMIKA_TURNSTILE_SITE_KEY", "the widget's site key")?,
            secret: Secret(Some(required(
                "KOMIKA_TURNSTILE_SECRET",
                "the widget's secret key",
            )?)),
        }))
    }
}

/// Why a mutation was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    /// No token came with the mutation.
    Missing,
    /// Cloudflare refused the token: invalid, expired or already spent.
    Refused,
    /// Cloudflare could not be asked, or refused the server's own secret.
    Unavailable,
}

/// The checker `main` attaches as schema data when Turnstile is on.
pub struct Turnstile {
    site_key: String,
    secret: String,
    verify_url: String,
    http: reqwest::Client,
}

#[derive(serde::Serialize)]
struct Request<'a> {
    secret: &'a str,
    response: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    remoteip: Option<&'a str>,
    idempotency_key: &'a str,
}

#[derive(serde::Deserialize)]
struct Answer {
    success: bool,
    #[serde(default, rename = "error-codes")]
    error_codes: Vec<String>,
}

impl Turnstile {
    pub fn new(config: &TurnstileConfig) -> Self {
        Self::with(config, SITEVERIFY_URL, ATTEMPT_TIMEOUT)
    }

    fn with(config: &TurnstileConfig, verify_url: &str, attempt_timeout: Duration) -> Self {
        Self {
            site_key: config.site_key.clone(),
            secret: config.secret.0.clone().unwrap_or_default(),
            verify_url: verify_url.to_string(),
            http: reqwest::Client::builder()
                .connect_timeout(CONNECT_TIMEOUT.min(attempt_timeout))
                .timeout(attempt_timeout)
                .build()
                .expect("reqwest client with timeouts"),
        }
    }

    pub fn site_key(&self) -> &str {
        &self.site_key
    }

    /// Check one token. `remote_ip` is the client address from the trusted-proxy logic
    /// (`CF-Connecting-IP` behind Cloudflare); `None` when the server does not know it.
    pub async fn verify(
        &self,
        token: Option<&str>,
        remote_ip: Option<&str>,
    ) -> Result<(), Rejection> {
        let token = token
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or(Rejection::Missing)?;
        if token.len() > MAX_TOKEN_LEN {
            return Err(Rejection::Refused);
        }
        let idempotency_key = uuid::Uuid::new_v4().to_string();
        let request = Request {
            secret: &self.secret,
            response: token,
            remoteip: remote_ip.filter(|ip| ip.parse::<std::net::IpAddr>().is_ok()),
            idempotency_key: &idempotency_key,
        };
        let mut last_error = String::new();
        for _ in 0..ATTEMPTS {
            let answer = match self.http.post(&self.verify_url).json(&request).send().await {
                Ok(response) if response.status().is_server_error() => {
                    last_error = format!("siteverify answered {}", response.status());
                    continue;
                }
                Ok(response) => response.json::<Answer>().await,
                Err(error) => {
                    last_error = error.to_string();
                    continue;
                }
            };
            let answer = match answer {
                Ok(answer) => answer,
                Err(error) => {
                    last_error = format!("unreadable siteverify answer: {error}");
                    continue;
                }
            };
            if answer.success {
                return Ok(());
            }
            let has = |code: &str| answer.error_codes.iter().any(|c| c == code);
            if has("internal-error") {
                last_error = "siteverify reported an internal error".into();
                continue;
            }
            if has("missing-input-secret") || has("invalid-input-secret") {
                tracing::error!(
                    error_codes = ?answer.error_codes,
                    "turnstile: Cloudflare refused the secret; check KOMIKA_TURNSTILE_SECRET"
                );
                return Err(Rejection::Unavailable);
            }
            tracing::info!(error_codes = ?answer.error_codes, "turnstile: token refused");
            return Err(Rejection::Refused);
        }
        tracing::warn!(error = %last_error, "turnstile: siteverify could not be asked");
        Err(Rejection::Unavailable)
    }
}

/// A local stand-in for `siteverify` that answers like Cloudflare does for its published
/// test secrets, so no test ever needs a real secret or the network.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
    use std::sync::{Arc, Mutex};

    /// Cloudflare's test secret that passes every token.
    pub const PASSING_SECRET: &str = "1x0000000000000000000000000000000AA";
    /// Cloudflare's test secret that refuses every token.
    pub const FAILING_SECRET: &str = "2x0000000000000000000000000000000AA";
    /// Cloudflare's test secret that reports the token as already spent.
    pub const SPENT_SECRET: &str = "3x0000000000000000000000000000000AA";
    /// Cloudflare's test site key whose widget always passes.
    pub const PASSING_SITE_KEY: &str = "1x00000000000000000000AA";
    /// The token Cloudflare's test site keys hand out.
    pub const DUMMY_TOKEN: &str = "XXXX.DUMMY.TOKEN.XXXX";

    #[derive(Default)]
    pub struct Mock {
        /// Every request body the stand-in received.
        pub requests: Mutex<Vec<serde_json::Value>>,
        /// Answer this many requests with a 502 first.
        pub fail_first: Mutex<usize>,
        /// Never answer (the client's timeout must end the call).
        pub hang: Mutex<bool>,
    }

    async fn siteverify(
        State(mock): State<Arc<Mock>>,
        Json(body): Json<serde_json::Value>,
    ) -> (StatusCode, Json<serde_json::Value>) {
        mock.requests.lock().unwrap().push(body.clone());
        if *mock.hang.lock().unwrap() {
            std::future::pending::<()>().await;
        }
        {
            let mut fail = mock.fail_first.lock().unwrap();
            if *fail > 0 {
                *fail -= 1;
                return (StatusCode::BAD_GATEWAY, Json(serde_json::json!({})));
            }
        }
        let answer = match body["secret"].as_str() {
            Some(PASSING_SECRET) => serde_json::json!({ "success": true, "error-codes": [] }),
            Some(FAILING_SECRET) => {
                serde_json::json!({ "success": false, "error-codes": ["invalid-input-response"] })
            }
            Some(SPENT_SECRET) => {
                serde_json::json!({ "success": false, "error-codes": ["timeout-or-duplicate"] })
            }
            _ => serde_json::json!({ "success": false, "error-codes": ["invalid-input-secret"] }),
        };
        (StatusCode::OK, Json(answer))
    }

    /// Start the stand-in; returns it and a checker that calls it with `secret`.
    pub async fn checker(secret: &str, attempt_timeout: Duration) -> (Arc<Mock>, Turnstile) {
        let mock = Arc::new(Mock::default());
        let app = Router::new()
            .route("/siteverify", post(siteverify))
            .with_state(mock.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/siteverify", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let config = TurnstileConfig {
            site_key: PASSING_SITE_KEY.into(),
            secret: Secret(Some(secret.into())),
        };
        (mock, Turnstile::with(&config, &url, attempt_timeout))
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use std::collections::HashMap;

    fn parse(vars: &[(&str, &str)]) -> Result<Option<TurnstileConfig>, String> {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        TurnstileConfig::parse(|name| vars.get(name).cloned())
    }

    #[test]
    fn off_unless_switched_on_and_on_needs_both_keys() {
        assert!(parse(&[]).unwrap().is_none());
        assert!(parse(&[("KOMIKA_TURNSTILE", "off")]).unwrap().is_none());
        // Keys alone do not switch it on.
        assert!(parse(&[("KOMIKA_TURNSTILE_SECRET", PASSING_SECRET)])
            .unwrap()
            .is_none());
        let error = parse(&[("KOMIKA_TURNSTILE", "on")]).unwrap_err();
        assert!(error.contains("KOMIKA_TURNSTILE_SITE_KEY"), "{error}");
        let error = parse(&[
            ("KOMIKA_TURNSTILE", "on"),
            ("KOMIKA_TURNSTILE_SITE_KEY", PASSING_SITE_KEY),
            ("KOMIKA_TURNSTILE_SECRET", "  "),
        ])
        .unwrap_err();
        assert_eq!(
            error,
            "KOMIKA_TURNSTILE=on needs KOMIKA_TURNSTILE_SECRET (the widget's secret key)"
        );
        let error = parse(&[("KOMIKA_TURNSTILE", "yes please")]).unwrap_err();
        assert!(error.contains("must be on or off"), "{error}");
        let config = parse(&[
            ("KOMIKA_TURNSTILE", "ON"),
            ("KOMIKA_TURNSTILE_SITE_KEY", PASSING_SITE_KEY),
            ("KOMIKA_TURNSTILE_SECRET", PASSING_SECRET),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(config.site_key, PASSING_SITE_KEY);
        // `Config` is logged at startup: the secret must not be in its Debug text.
        assert!(!format!("{config:?}").contains(PASSING_SECRET));
    }

    #[tokio::test]
    async fn a_passing_token_is_sent_once_with_the_client_address() {
        let (mock, check) = checker(PASSING_SECRET, ATTEMPT_TIMEOUT).await;
        assert_eq!(
            check.verify(Some(DUMMY_TOKEN), Some("203.0.113.7")).await,
            Ok(())
        );
        let requests = mock.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["response"], DUMMY_TOKEN);
        assert_eq!(requests[0]["secret"], PASSING_SECRET);
        assert_eq!(requests[0]["remoteip"], "203.0.113.7");
        let key = requests[0]["idempotency_key"].as_str().unwrap();
        assert!(uuid::Uuid::parse_str(key).is_ok(), "{key}");
    }

    #[tokio::test]
    async fn missing_failed_and_spent_tokens_are_refused() {
        let (mock, check) = checker(FAILING_SECRET, ATTEMPT_TIMEOUT).await;
        assert_eq!(check.verify(None, None).await, Err(Rejection::Missing));
        assert_eq!(
            check.verify(Some("  "), None).await,
            Err(Rejection::Missing)
        );
        // Nothing is asked for a missing token or one longer than any real token.
        let long = "x".repeat(MAX_TOKEN_LEN + 1);
        assert_eq!(
            check.verify(Some(&long), None).await,
            Err(Rejection::Refused)
        );
        assert!(mock.requests.lock().unwrap().is_empty());
        assert_eq!(
            check.verify(Some(DUMMY_TOKEN), Some("unknown")).await,
            Err(Rejection::Refused)
        );
        // An address the server could not resolve is left out, not sent as text.
        assert!(mock.requests.lock().unwrap()[0].get("remoteip").is_none());

        let (_, spent) = checker(SPENT_SECRET, ATTEMPT_TIMEOUT).await;
        assert_eq!(
            spent.verify(Some(DUMMY_TOKEN), None).await,
            Err(Rejection::Refused)
        );
        // A wrong secret is the server's fault, not the visitor's.
        let (_, wrong) = checker("not-a-secret", ATTEMPT_TIMEOUT).await;
        assert_eq!(
            wrong.verify(Some(DUMMY_TOKEN), None).await,
            Err(Rejection::Unavailable)
        );
    }

    #[tokio::test]
    async fn a_failed_call_is_retried_once_with_the_same_idempotency_key() {
        let (mock, check) = checker(PASSING_SECRET, ATTEMPT_TIMEOUT).await;
        *mock.fail_first.lock().unwrap() = 1;
        assert_eq!(check.verify(Some(DUMMY_TOKEN), None).await, Ok(()));
        {
            let requests = mock.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert_eq!(
                requests[0]["idempotency_key"],
                requests[1]["idempotency_key"]
            );
        }
        // Two failures in a row: refused, not let through, after exactly two calls.
        *mock.fail_first.lock().unwrap() = 2;
        mock.requests.lock().unwrap().clear();
        assert_eq!(
            check.verify(Some(DUMMY_TOKEN), None).await,
            Err(Rejection::Unavailable)
        );
        assert_eq!(mock.requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_silent_siteverify_ends_in_the_timeout() {
        let (mock, check) = checker(PASSING_SECRET, Duration::from_millis(200)).await;
        *mock.hang.lock().unwrap() = true;
        let started = std::time::Instant::now();
        assert_eq!(
            check.verify(Some(DUMMY_TOKEN), None).await,
            Err(Rejection::Unavailable)
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        // An unreachable address is refused the same way.
        let config = TurnstileConfig {
            site_key: PASSING_SITE_KEY.into(),
            secret: Secret(Some(PASSING_SECRET.into())),
        };
        let nowhere = Turnstile::with(&config, "http://127.0.0.1:1/siteverify", ATTEMPT_TIMEOUT);
        assert_eq!(
            nowhere.verify(Some(DUMMY_TOKEN), None).await,
            Err(Rejection::Unavailable)
        );
    }
}
