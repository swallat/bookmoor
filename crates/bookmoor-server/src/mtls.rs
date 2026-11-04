//! Optional client-certificate requirement, enforced by the reverse proxy.
//!
//! The server never sees the TLS handshake (it sits behind a proxy), so the
//! proxy verifies the certificate against its client CA and reports the
//! outcome in a request header, for example nginx `$ssl_client_verify`
//! (`SUCCESS`). With a check configured, every request except `/health`
//! must carry that header with the expected value, otherwise it gets 403
//! before any token is looked at. This is a second factor next to the
//! bearer token, not a replacement.
//!
//! The proxy must overwrite or strip the header on every request, and the
//! port must not be reachable around the proxy; otherwise a client could
//! send the header itself.

use axum::extract::{Request, State};
use axum::http::{HeaderName, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use subtle::ConstantTimeEq;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientCertCheck {
    header: HeaderName,
    /// `None` accepts any non-empty value (proxies that only set the header
    /// when a verified certificate was presented).
    value: Option<String>,
}

impl ClientCertCheck {
    /// `NAME`, `NAME=VALUE` or `NAME=*`. `NAME` alone expects `SUCCESS`,
    /// what nginx and Apache report for a verified certificate.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let (name, value) = match spec.split_once('=') {
            Some((n, "*")) => (n, None),
            Some((n, v)) if !v.trim().is_empty() => (n, Some(v.trim().to_string())),
            Some(_) => return Err("the expected header value must not be empty".into()),
            None => (spec, Some("SUCCESS".to_string())),
        };
        let header = HeaderName::from_bytes(name.trim().as_bytes())
            .map_err(|_| format!("{:?} is not a valid header name", name.trim()))?;
        Ok(Self { header, value })
    }

    fn accepts(&self, headers: &axum::http::HeaderMap) -> bool {
        // A header sent twice is ambiguous: refuse unless every copy is right.
        let mut seen = false;
        for v in headers.get_all(&self.header) {
            seen = true;
            let ok = match &self.value {
                None => v.to_str().is_ok_and(|t| {
                    let t = t.trim();
                    // nginx and Apache send these when there is no good certificate.
                    !t.is_empty()
                        && !matches!(t, "NONE" | "0" | "(null)")
                        && !t.starts_with("FAILED")
                }),
                Some(want) => bool::from(v.as_bytes().ct_eq(want.as_bytes())),
            };
            if !ok {
                return false;
            }
        }
        seen
    }
}

pub(crate) async fn require_client_cert(
    State(check): State<ClientCertCheck>,
    req: Request,
    next: Next,
) -> Response {
    if req.uri().path() == "/health" || check.accepts(req.headers()) {
        return next.run(req).await;
    }
    (
        StatusCode::FORBIDDEN,
        axum::Json(serde_json::json!({
            "error": "a valid client certificate is required"
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn any_value_mode_still_refuses_failure_verdicts() {
        let c = ClientCertCheck::parse("X-Cert=*").unwrap();
        let with = |v: &'static str| {
            let mut h = axum::http::HeaderMap::new();
            h.insert("x-cert", v.parse().unwrap());
            h
        };
        assert!(c.accepts(&with("MIIB...")));
        assert!(c.accepts(&with("SUCCESS")));
        for bad in ["NONE", "FAILED:expired", "0", " "] {
            assert!(!c.accepts(&with(bad)), "{bad}");
        }
        assert!(!c.accepts(&axum::http::HeaderMap::new()));
    }

    #[test]
    fn parses_the_spec() {
        let c = ClientCertCheck::parse("X-SSL-Client-Verify").unwrap();
        assert_eq!(c.value.as_deref(), Some("SUCCESS"));
        assert_eq!(ClientCertCheck::parse("X-Cert=*").unwrap().value, None);
        assert_eq!(
            ClientCertCheck::parse("X-Cert=ok")
                .unwrap()
                .value
                .as_deref(),
            Some("ok")
        );
        assert!(ClientCertCheck::parse("bad name").is_err());
        assert!(ClientCertCheck::parse("X-Cert=").is_err());
    }
}
