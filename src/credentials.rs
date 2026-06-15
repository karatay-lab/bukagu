//! API credentials for fetching the backup recipient key.
//!
//! The API token (copied once from the user's website) and the API base URL live
//! in `~/.config/bukagu/credentials.json` with `0600` permissions. The
//! environment variables [`TOKEN_ENV`] / [`URL_ENV`] override the file (handy for
//! CI). A project-local `.env` is auto-loaded into the environment at startup
//! (see `main::run`), so `BUKAGU_API_TOKEN=…` in `.env` is honored too — but a
//! real exported variable still wins, and `.env` is gitignored. Credentials are
//! **never** written into the repo's `.bukagu/` store, and the token is never
//! logged or printed.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

/// Environment variable that overrides the stored API token.
pub const TOKEN_ENV: &str = "BUKAGU_API_TOKEN";
/// Environment variable that overrides the stored API base URL.
pub const URL_ENV: &str = "BUKAGU_API_URL";
/// Environment variable that overrides the recipient-endpoint path (default
/// `/recipient`), for APIs that serve the key under a different route. Resolved
/// in [`crate::backup::key`].
pub const RECIPIENT_PATH_ENV: &str = "BUKAGU_API_RECIPIENT_PATH";

/// Resolved, ready-to-use credentials: a token plus a validated https base URL
/// (trimmed, no trailing slash).
#[derive(Debug, Clone)]
pub struct Credentials {
    pub api_token: String,
    pub api_url: String,
}

/// On-disk shape of `credentials.json`. Both fields are optional so a partially
/// filled or hand-edited file still parses.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct StoredCredentials {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    api_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    api_url: Option<String>,
}

impl Credentials {
    /// Resolve credentials from the environment (highest priority) then the
    /// config file. Errors with a setup hint if no token/URL is configured.
    pub fn load() -> Result<Self> {
        let stored = read_file(&config_path()?)?;
        resolve(stored, env_var(TOKEN_ENV), env_var(URL_ENV))
    }

    /// Save `api_token` + `api_url` to the config file (mode `0600`), creating
    /// `~/.config/bukagu/` if needed. Validates the URL is https first. Returns
    /// the path written.
    pub fn save(api_token: &str, api_url: &str) -> Result<PathBuf> {
        let api_url = validate_url(api_url)?;
        let api_token = api_token.trim().to_string();
        if api_token.is_empty() {
            bail!("the API token is empty");
        }
        let path = config_path()?;
        write_file(
            &path,
            &StoredCredentials {
                api_token: Some(api_token),
                api_url: Some(api_url),
            },
        )?;
        Ok(path)
    }
}

/// `~/.config/bukagu/credentials.json` (honoring `$XDG_CONFIG_HOME`).
pub fn config_path() -> Result<PathBuf> {
    let dir =
        dirs::config_dir().context("could not determine your config directory (is $HOME set?)")?;
    Ok(dir.join("bukagu").join("credentials.json"))
}

/// The non-empty, trimmed value of environment variable `name`, if set.
fn env_var(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Resolve env-over-file precedence into ready credentials. Pure (no I/O) so it
/// is unit-testable.
fn resolve(
    stored: StoredCredentials,
    env_token: Option<String>,
    env_url: Option<String>,
) -> Result<Credentials> {
    let api_token = env_token
        .or(stored.api_token.filter(|s| !s.trim().is_empty()))
        .context(
            "no API token found — set BUKAGU_API_TOKEN, or run `bukagu auth login` \
             after copying your token from the website",
        )?
        .trim()
        .to_string();
    let raw_url = env_url
        .or(stored.api_url.filter(|s| !s.trim().is_empty()))
        .context("no API URL found — set BUKAGU_API_URL, or run `bukagu auth login`")?;
    let api_url = validate_url(&raw_url)?;
    Ok(Credentials { api_token, api_url })
}

/// Validate that a base URL is HTTPS and return it trimmed (no trailing slash).
/// Rejecting plain HTTP is a security guard: the token must only travel over TLS.
pub fn validate_url(raw: &str) -> Result<String> {
    let url = raw.trim().trim_end_matches('/');
    if let Some(host) = url.strip_prefix("https://") {
        if host.is_empty() {
            bail!("the API URL has no host: {raw:?}");
        }
        Ok(url.to_string())
    } else if url.starts_with("http://") {
        bail!(
            "refusing a plain-HTTP API URL ({raw:?}) — use https:// so the token is encrypted in transit"
        );
    } else {
        bail!("the API URL must start with https:// (got {raw:?})");
    }
}

/// Read and parse the credentials file; an absent file yields empty defaults.
fn read_file(path: &Path) -> Result<StoredCredentials> {
    match fs::read_to_string(path) {
        Ok(data) => {
            serde_json::from_str(&data).with_context(|| format!("parsing {}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(StoredCredentials::default()),
        Err(e) => Err(anyhow!(e)).with_context(|| format!("reading {}", path.display())),
    }
}

/// Write the credentials file, restricted to the owner (`0600` on unix), creating
/// the parent directory as needed.
fn write_file(path: &Path, creds: &StoredCredentials) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(creds).context("serializing credentials")?;
    write_owner_only(path, json.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Create/overwrite `path` with `bytes`, owner-read/write only.
#[cfg(unix)]
fn write_owner_only(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    // `mode(0o600)` only applies when *creating* the file, so also fix up the
    // permissions afterwards in case the file already existed with looser bits.
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn write_owner_only(path: &Path, bytes: &[u8]) -> Result<()> {
    // On non-unix we rely on the user's profile-directory ACLs for protection.
    fs::write(path, bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn stored(token: &str, url: &str) -> StoredCredentials {
        StoredCredentials {
            api_token: Some(token.into()),
            api_url: Some(url.into()),
        }
    }

    #[test]
    fn env_overrides_file() {
        let creds = resolve(
            stored("file-token", "https://file.example.com"),
            Some("env-token".into()),
            Some("https://env.example.com/".into()),
        )
        .unwrap();
        assert_eq!(creds.api_token, "env-token");
        assert_eq!(
            creds.api_url, "https://env.example.com",
            "trailing slash trimmed"
        );
    }

    #[test]
    fn file_used_when_env_absent() {
        let creds = resolve(stored("file-token", "https://file.example.com"), None, None).unwrap();
        assert_eq!(creds.api_token, "file-token");
        assert_eq!(creds.api_url, "https://file.example.com");
    }

    #[test]
    fn missing_token_is_an_error() {
        let err = resolve(
            StoredCredentials::default(),
            None,
            Some("https://x.example".into()),
        )
        .unwrap_err();
        assert!(err.to_string().contains("no API token"));
    }

    #[test]
    fn http_url_is_rejected() {
        assert!(validate_url("http://insecure.example.com").is_err());
        assert!(validate_url("ftp://x").is_err());
        assert!(validate_url("https://").is_err(), "https with no host");
        assert_eq!(
            validate_url("  https://api.example.com/  ").unwrap(),
            "https://api.example.com"
        );
    }

    #[test]
    fn read_file_absent_is_empty() {
        let tmp = tempdir().unwrap();
        let got = read_file(&tmp.path().join("nope.json")).unwrap();
        assert!(got.api_token.is_none() && got.api_url.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn write_file_is_owner_only_and_roundtrips() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempdir().unwrap();
        let path = tmp.path().join("bukagu/credentials.json");
        write_file(&path, &stored("tok", "https://api.example.com")).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "credentials file must be owner-only");

        let creds = resolve(read_file(&path).unwrap(), None, None).unwrap();
        assert_eq!(creds.api_token, "tok");
        assert_eq!(creds.api_url, "https://api.example.com");
    }
}
