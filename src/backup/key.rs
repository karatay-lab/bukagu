//! Obtaining the public age recipient that a backup is encrypted to.
//!
//! [`HttpKeyProvider`] fetches it over HTTPS from the user's API (the live path);
//! [`StaticKeyProvider`] returns a fixed recipient for tests and offline
//! development. Both go through the [`KeyProvider`] trait so the backup engine is
//! agnostic to where the key came from.

use std::time::Duration;

use age::x25519::Recipient;
use anyhow::{Context, Result, anyhow, bail};

use crate::credentials::Credentials;

/// `User-Agent` sent with key requests.
const USER_AGENT: &str = concat!("bukagu/", env!("CARGO_PKG_VERSION"));

/// Default path appended to the API base URL to fetch the recipient. Overridable
/// via [`crate::credentials::RECIPIENT_PATH_ENV`].
const DEFAULT_RECIPIENT_PATH: &str = "/recipient";

/// Normalize a recipient-path override: trim whitespace and any trailing `/`,
/// add a leading `/` if missing, and fall back to [`DEFAULT_RECIPIENT_PATH`] when
/// empty. Kept pure (no env access) so it's unit-testable.
fn normalize_recipient_path(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        DEFAULT_RECIPIENT_PATH.to_string()
    } else if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{trimmed}")
    }
}

/// The recipient-endpoint path from the environment, or the default.
fn recipient_path() -> String {
    let raw = std::env::var(crate::credentials::RECIPIENT_PATH_ENV).unwrap_or_default();
    normalize_recipient_path(&raw)
}

/// Supplies the public age recipient to encrypt a backup to.
pub trait KeyProvider {
    /// Return the recipient (public key). May perform network I/O.
    fn recipient(&self) -> Result<Recipient>;
}

/// Fetches the recipient over HTTPS from the user's API, authenticating with a
/// bearer token. The response body must be a single `age1…` recipient string.
pub struct HttpKeyProvider {
    creds: Credentials,
}

impl HttpKeyProvider {
    pub fn new(creds: Credentials) -> Self {
        Self { creds }
    }

    /// `GET {api_url}{path}` — `api_url` is already validated https with no
    /// trailing slash (see [`crate::credentials::validate_url`]); `path` defaults
    /// to `/recipient` but is overridable (see [`recipient_path`]).
    fn endpoint(&self) -> String {
        format!("{}{}", self.creds.api_url, recipient_path())
    }
}

impl KeyProvider for HttpKeyProvider {
    fn recipient(&self) -> Result<Recipient> {
        let url = self.endpoint();
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout(Duration::from_secs(20))
            // Never follow redirects: the bearer token must not be replayed to
            // some other host the API might 3xx us toward.
            .redirects(0)
            .user_agent(USER_AGENT)
            .build();

        let response = match agent
            .get(&url)
            .set("Authorization", &format!("Bearer {}", self.creds.api_token))
            .set("Accept", "text/plain")
            .call()
        {
            Ok(resp) => resp,
            // Deliberately do not echo the response body — it could carry sensitive detail.
            Err(ureq::Error::Status(code, _)) => {
                let hint = match code {
                    401 | 403 => " — is your API token valid? re-run `bukagu auth login`",
                    300..=399 => {
                        " — the API redirected; point BUKAGU_API_URL at the final https URL"
                    }
                    _ => "",
                };
                bail!("the backup key API returned HTTP {code}{hint}");
            }
            Err(ureq::Error::Transport(t)) => {
                return Err(anyhow!(t.to_string()))
                    .with_context(|| format!("could not reach the backup key API at {url}"));
            }
        };

        let body = response
            .into_string()
            .context("reading the key API response")?;
        parse_recipient(&body)
    }
}

/// A fixed recipient — for tests and offline development (the `--recipient` dev
/// flag). Never touches the network.
pub struct StaticKeyProvider {
    recipient: Recipient,
}

impl StaticKeyProvider {
    pub fn new(recipient: Recipient) -> Self {
        Self { recipient }
    }
}

impl KeyProvider for StaticKeyProvider {
    fn recipient(&self) -> Result<Recipient> {
        Ok(self.recipient.clone())
    }
}

/// Parse and validate an `age1…` recipient from an API response body. Rejecting
/// anything malformed means a hostile or buggy response can't silently produce a
/// backup encrypted to the wrong (or no) key.
pub fn parse_recipient(body: &str) -> Result<Recipient> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        bail!("the backup key API returned an empty body");
    }
    trimmed
        .parse::<Recipient>()
        .map_err(|e| anyhow!("the backup key API did not return a valid age recipient: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use age::x25519::Identity;

    #[test]
    fn parse_recipient_accepts_a_real_age_key() {
        let recipient = Identity::generate().to_public();
        let text = recipient.to_string();
        let parsed = parse_recipient(&format!("  {text}\n")).unwrap();
        assert_eq!(
            parsed.to_string(),
            text,
            "whitespace trimmed, key preserved"
        );
    }

    #[test]
    fn parse_recipient_rejects_junk() {
        assert!(parse_recipient("").is_err());
        assert!(parse_recipient("   ").is_err());
        assert!(parse_recipient("not-an-age-key").is_err());
        // Looks like a recipient (age1…) but is not valid bech32.
        assert!(parse_recipient("age1clearlyinvalid").is_err());
    }

    #[test]
    fn recipient_path_is_normalized() {
        assert_eq!(
            normalize_recipient_path(""),
            "/recipient",
            "empty → default"
        );
        assert_eq!(normalize_recipient_path("   "), "/recipient");
        assert_eq!(
            normalize_recipient_path("/"),
            "/recipient",
            "bare slash → default"
        );
        assert_eq!(normalize_recipient_path("/recipient"), "/recipient");
        assert_eq!(
            normalize_recipient_path("recipient"),
            "/recipient",
            "leading slash added"
        );
        assert_eq!(
            normalize_recipient_path("/v2/key/"),
            "/v2/key",
            "trailing slash trimmed"
        );
        assert_eq!(
            normalize_recipient_path("  /api/recipient  "),
            "/api/recipient"
        );
    }

    #[test]
    fn static_provider_returns_its_recipient() {
        let recipient = Identity::generate().to_public();
        let provider = StaticKeyProvider::new(recipient.clone());
        assert_eq!(
            provider.recipient().unwrap().to_string(),
            recipient.to_string()
        );
    }
}
