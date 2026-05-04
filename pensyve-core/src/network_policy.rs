//! Network policy — fail-closed by default (Rev B §5.8 contract, lifted into v2.1).
//!
//! `NetworkPolicy` gates every outbound HTTP call inside pensyve-core that
//! reaches an LLM, extractor, or classifier endpoint. The default
//! ([`NetworkPolicy::Disabled`]) rejects every URL — even localhost — so
//! a fresh `Pensyve` constructed without explicit policy opt-in cannot
//! reach the network. Higher-level crates (`pensyve-mcp` stdio,
//! `pensyve-cli`, `pensyve-benchmarks`) relax to `LocalOnly` with a
//! configured local-LLM endpoint; `pensyve-mcp-gateway` opts into
//! `Permissive` for the managed-service path. See
//! `pensyve-docs/specs/2026-05-04-pensyve-v2.1-ship.md` §5 for the
//! per-crate default matrix and the gateway infrastructure-HTTP carve-out.
//!
//! Carve-out (CRITICAL): `NetworkPolicy` gates pensyve-core LLM /
//! extractor traffic only. It does NOT gate `pensyve-mcp-gateway`
//! infrastructure HTTP (OAuth, Stripe metering, auth provider). Those
//! callers do not consult `NetworkPolicy` — see v2.1 spec §5.3 for the
//! architectural reason. Without this carve-out the gateway would be
//! forced to `Permissive` purely to keep OAuth working, defeating the
//! safety property of the LLM path.

use std::str::FromStr;

use thiserror::Error;

/// Outbound-network policy applied at every pensyve-core LLM / extractor
/// callsite.
///
/// `Disabled` is the default. `LocalOnly { url }` allows traffic to the
/// configured `url` only — typical for embedded local-vLLM setups.
/// `Permissive` allows any URL and is intended for the managed-service
/// path only.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum NetworkPolicy {
    /// Default — every network call returns [`NetworkRequiredError`].
    #[default]
    Disabled,
    /// Allow only the configured URL. The check is exact-match on the
    /// `<scheme>://<host>[:port]` prefix of `target_url`. Path differences
    /// are ignored so a single allowed base-URL covers all sub-paths
    /// (e.g., `/chat/completions` and `/messages` against the same vLLM).
    LocalOnly {
        /// Allowed URL prefix. Typically the local-LLM endpoint, e.g.
        /// `http://localhost:8888/v1`.
        url: String,
    },
    /// Allow any URL. Managed-service mode — explicit opt-in only.
    Permissive,
}

impl NetworkPolicy {
    /// Returns `Ok(())` if the policy allows `target_url`,
    /// `Err(NetworkRequiredError)` otherwise.
    ///
    /// `target_url` may be a full URL with path / query (e.g.
    /// `http://localhost:8888/v1/chat/completions`); only the
    /// scheme + authority is compared against `LocalOnly.url`.
    pub fn check(&self, target_url: &str) -> Result<(), NetworkRequiredError> {
        match self {
            Self::Disabled => Err(NetworkRequiredError {
                target: target_url.to_string(),
                policy: "Disabled".into(),
            }),
            Self::LocalOnly { url } => {
                if matches_authority(target_url, url) {
                    Ok(())
                } else {
                    Err(NetworkRequiredError {
                        target: target_url.to_string(),
                        policy: format!("LocalOnly {{ url: {url:?} }}"),
                    })
                }
            }
            Self::Permissive => Ok(()),
        }
    }

    /// Parse from the `PENSYVE_NETWORK_POLICY` env var. Returns `None`
    /// when the var is unset.
    ///
    /// Recognized values: `disabled`, `local-only` (or `localonly`),
    /// `permissive`. Case-insensitive. For `local-only`, the URL is
    /// taken from the second positional argument; callers should pass
    /// the LLM endpoint's base URL.
    ///
    /// ```text
    /// PENSYVE_NETWORK_POLICY=disabled   → NetworkPolicy::Disabled
    /// PENSYVE_NETWORK_POLICY=local-only → NetworkPolicy::LocalOnly { url: <fallback_url> }
    /// PENSYVE_NETWORK_POLICY=permissive → NetworkPolicy::Permissive
    /// (unset)                           → None
    /// ```
    pub fn from_env(fallback_local_url: &str) -> Option<Self> {
        let raw = std::env::var("PENSYVE_NETWORK_POLICY").ok()?;
        Self::parse(&raw, fallback_local_url).ok()
    }

    fn parse(raw: &str, fallback_local_url: &str) -> Result<Self, NetworkPolicyParseError> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "disabled" | "off" => Ok(Self::Disabled),
            "local-only" | "localonly" | "local" => Ok(Self::LocalOnly {
                url: fallback_local_url.to_string(),
            }),
            "permissive" | "any" => Ok(Self::Permissive),
            other => Err(NetworkPolicyParseError(other.to_string())),
        }
    }
}

impl FromStr for NetworkPolicy {
    type Err = NetworkPolicyParseError;

    /// Parse without a fallback URL. `local-only` returns
    /// `LocalOnly { url: "" }` which will reject every target — callers
    /// usually want [`NetworkPolicy::from_env`] or `parse` with a
    /// fallback URL instead.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s, "")
    }
}

/// Returned when an outbound network call is rejected by the active
/// [`NetworkPolicy`]. Callers typically wrap this in their domain error
/// (`ExtractionError::Transport`, `ClassifierError::Transport`, etc.) and
/// surface it to the operator.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
#[error("network call to {target} not permitted by NetworkPolicy::{policy}")]
pub struct NetworkRequiredError {
    pub target: String,
    pub policy: String,
}

/// Error from parsing a `NetworkPolicy` string (env var, CLI flag, etc.).
#[derive(Debug, Clone, Error, PartialEq, Eq)]
#[error("unrecognized NetworkPolicy {0:?}; expected one of: disabled, local-only, permissive")]
pub struct NetworkPolicyParseError(pub String);

/// Compare the scheme+authority of `target_url` against the prefix of
/// `allowed_url`. Both are matched lower-case on the scheme and host;
/// port is exact-match. Path / query / fragment are ignored.
///
/// Returns false when either input cannot be split into scheme + host.
fn matches_authority(target_url: &str, allowed_url: &str) -> bool {
    let Some(t) = scheme_authority(target_url) else {
        return false;
    };
    let Some(a) = scheme_authority(allowed_url) else {
        return false;
    };
    t == a
}

/// Extract the lower-cased `<scheme>://<host>[:port]` of `url`. Returns
/// `None` if `url` doesn't have a `://` separator or has an empty host.
fn scheme_authority(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme.is_empty() {
        return None;
    }
    // host[:port] ends at the first '/', '?', or '#'
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    if authority.is_empty() {
        return None;
    }
    Some(format!(
        "{}://{}",
        scheme.to_ascii_lowercase(),
        authority.to_ascii_lowercase()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_disabled() {
        assert_eq!(NetworkPolicy::default(), NetworkPolicy::Disabled);
    }

    #[test]
    fn disabled_rejects_every_url() {
        let p = NetworkPolicy::Disabled;
        assert!(
            p.check("http://localhost:8888/v1/chat/completions")
                .is_err()
        );
        assert!(p.check("https://cloud.example.com/v1/messages").is_err());
        assert!(p.check("http://example.com/").is_err());
    }

    #[test]
    fn permissive_allows_every_url() {
        let p = NetworkPolicy::Permissive;
        assert!(p.check("http://localhost:8888/v1/chat/completions").is_ok());
        assert!(p.check("https://cloud.example.com/v1/messages").is_ok());
        assert!(p.check("http://example.com/").is_ok());
    }

    #[test]
    fn local_only_matches_authority_ignoring_path() {
        let p = NetworkPolicy::LocalOnly {
            url: "http://localhost:8888/v1".into(),
        };
        // Same authority, different path — allowed.
        assert!(p.check("http://localhost:8888/v1/chat/completions").is_ok());
        assert!(p.check("http://localhost:8888/v1/embeddings").is_ok());
        assert!(p.check("http://localhost:8888/").is_ok());
    }

    #[test]
    fn local_only_rejects_other_authorities() {
        let p = NetworkPolicy::LocalOnly {
            url: "http://localhost:8888/v1".into(),
        };
        // Different host.
        assert!(
            p.check("http://127.0.0.1:8888/v1/chat/completions")
                .is_err()
        );
        // Different port.
        assert!(
            p.check("http://localhost:9999/v1/chat/completions")
                .is_err()
        );
        // Different scheme.
        assert!(
            p.check("https://localhost:8888/v1/chat/completions")
                .is_err()
        );
        // Cloud endpoint.
        assert!(p.check("https://cloud.example.com/v1/messages").is_err());
    }

    #[test]
    fn local_only_case_insensitive_host() {
        let p = NetworkPolicy::LocalOnly {
            url: "HTTP://LOCALHOST:8888/v1".into(),
        };
        assert!(p.check("http://localhost:8888/v1/chat/completions").is_ok());
        assert!(p.check("http://LocalHost:8888/foo").is_ok());
    }

    #[test]
    fn matches_authority_handles_query_and_fragment() {
        let p = NetworkPolicy::LocalOnly {
            url: "http://localhost:8888".into(),
        };
        assert!(p.check("http://localhost:8888/v1?stream=true").is_ok());
        assert!(p.check("http://localhost:8888/v1#frag").is_ok());
    }

    #[test]
    fn malformed_url_is_rejected_by_local_only() {
        let p = NetworkPolicy::LocalOnly {
            url: "http://localhost:8888".into(),
        };
        // No scheme separator.
        assert!(p.check("localhost:8888/v1").is_err());
        assert!(p.check("").is_err());
    }

    #[test]
    fn error_message_includes_target_and_policy() {
        let p = NetworkPolicy::Disabled;
        let err = p.check("http://example.com/x").unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("http://example.com/x"),
            "missing target: {msg}"
        );
        assert!(msg.contains("Disabled"), "missing policy: {msg}");

        let p = NetworkPolicy::LocalOnly {
            url: "http://localhost:8888".into(),
        };
        let err = p.check("https://cloud.example.com").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("LocalOnly"), "missing policy variant: {msg}");
        assert!(msg.contains("localhost"), "missing allowed url: {msg}");
    }

    #[test]
    fn parse_recognizes_canonical_names() {
        assert_eq!(
            NetworkPolicy::parse("disabled", "http://localhost:8888/v1"),
            Ok(NetworkPolicy::Disabled)
        );
        assert_eq!(
            NetworkPolicy::parse("local-only", "http://localhost:8888/v1"),
            Ok(NetworkPolicy::LocalOnly {
                url: "http://localhost:8888/v1".into()
            })
        );
        assert_eq!(
            NetworkPolicy::parse("permissive", "http://localhost:8888/v1"),
            Ok(NetworkPolicy::Permissive)
        );
    }

    #[test]
    fn parse_is_case_insensitive_and_accepts_aliases() {
        assert_eq!(
            NetworkPolicy::parse("DISABLED", ""),
            Ok(NetworkPolicy::Disabled)
        );
        assert_eq!(NetworkPolicy::parse("off", ""), Ok(NetworkPolicy::Disabled));
        assert_eq!(
            NetworkPolicy::parse("LocalOnly", "http://x:1"),
            Ok(NetworkPolicy::LocalOnly {
                url: "http://x:1".into()
            })
        );
        assert_eq!(
            NetworkPolicy::parse("any", ""),
            Ok(NetworkPolicy::Permissive)
        );
    }

    #[test]
    fn parse_rejects_unknown() {
        assert!(matches!(
            NetworkPolicy::parse("strict", ""),
            Err(NetworkPolicyParseError(_))
        ));
        assert!(matches!(
            NetworkPolicy::parse("", ""),
            Err(NetworkPolicyParseError(_))
        ));
    }

    #[test]
    fn from_str_uses_empty_fallback_url() {
        let p: NetworkPolicy = "local-only".parse().unwrap();
        assert_eq!(p, NetworkPolicy::LocalOnly { url: String::new() });
        // An empty URL rejects every target — callers should use from_env
        // (or parse with a real fallback) instead.
        assert!(p.check("http://anywhere/").is_err());
    }
}
