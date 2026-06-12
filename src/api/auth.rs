use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};

use super::models::{TokensResponse, User};

/// Margin before access-token expiry at which we refresh proactively,
/// mirroring the Android/macOS clients.
pub const EXPIRY_SKEW_SECONDS: i64 = 60;

/// Persisted session, same shape as the macOS client's AuthSession.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthSession {
    pub server_base_url: String,
    pub user: User,
    pub access_token: String,
    pub refresh_token: String,
    pub token_type: String,
    pub expires_at_epoch_seconds: i64,
}

impl AuthSession {
    pub fn new(server_base_url: String, user: User, tokens: TokensResponse) -> Self {
        Self {
            server_base_url,
            user,
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            token_type: tokens.token_type,
            expires_at_epoch_seconds: now_epoch_seconds() + tokens.expires_in_seconds,
        }
    }

    pub fn apply_tokens(&mut self, tokens: TokensResponse) {
        self.access_token = tokens.access_token;
        self.refresh_token = tokens.refresh_token;
        self.token_type = tokens.token_type;
        self.expires_at_epoch_seconds = now_epoch_seconds() + tokens.expires_in_seconds;
    }

    pub fn access_token_expired(&self) -> bool {
        now_epoch_seconds() + EXPIRY_SKEW_SECONDS >= self.expires_at_epoch_seconds
    }

    pub fn seconds_until_access_expiry(&self) -> i64 {
        self.expires_at_epoch_seconds - now_epoch_seconds()
    }
}

pub fn now_epoch_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn session_path() -> Option<PathBuf> {
    crate::config::project_dirs().map(|dirs| dirs.config_dir().join("credentials.json"))
}

pub fn load_session() -> Option<AuthSession> {
    let Some(path) = session_path() else {
        tracing::warn!("cannot determine config directory; no stored auth session loaded");
        return None;
    };
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => {
            tracing::debug!(path = %path.display(), "credentials file not found");
            return None;
        }
        Err(err) => {
            tracing::warn!(path = %path.display(), %err, "failed to read credentials file");
            return None;
        }
    };
    match serde_json::from_str::<AuthSession>(&text) {
        Ok(session) => {
            tracing::info!(
                path = %path.display(),
                user_id = session.user.id,
                user = %session.user.name,
                server = %session.server_base_url,
                expires_at_epoch_seconds = session.expires_at_epoch_seconds,
                seconds_until_expiry = session.seconds_until_access_expiry(),
                "loaded stored auth session"
            );
            Some(session)
        }
        Err(err) => {
            tracing::warn!(path = %path.display(), %err, "ignoring unreadable credentials file");
            None
        }
    }
}

pub fn save_session(session: &AuthSession) -> Result<()> {
    let path = session_path().context("cannot determine config directory")?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(session)?;
    tracing::info!(
        path = %path.display(),
        user_id = session.user.id,
        user = %session.user.name,
        server = %session.server_base_url,
        expires_at_epoch_seconds = session.expires_at_epoch_seconds,
        seconds_until_expiry = session.seconds_until_access_expiry(),
        "persisting auth session"
    );
    write_private(&path, &text).with_context(|| format!("writing {}", path.display()))?;
    tracing::debug!(path = %path.display(), "auth session persisted");
    Ok(())
}

pub fn delete_session() {
    if let Some(path) = session_path() {
        match fs::remove_file(&path) {
            Ok(()) => tracing::info!(path = %path.display(), "deleted stored auth session"),
            Err(err) if err.kind() == ErrorKind::NotFound => {
                tracing::debug!(path = %path.display(), "stored auth session already absent");
            }
            Err(err) => {
                tracing::warn!(path = %path.display(), %err, "failed to delete stored auth session")
            }
        }
    }
}

#[cfg(unix)]
fn write_private(path: &PathBuf, text: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(text.as_bytes())
}

#[cfg(not(unix))]
fn write_private(path: &PathBuf, text: &str) -> std::io::Result<()> {
    fs::write(path, text)
}

/// Same normalization rules as the Android client's ServerConfig:
/// add https:// when no scheme, require http(s) with a host, reject
/// credentials/query/fragment, lowercase the host, trim trailing slashes.
pub fn normalize_base_url(raw: &str) -> Result<String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        bail!("server URL is empty");
    }
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let url = reqwest::Url::parse(&with_scheme).context("invalid server URL")?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("server URL must use http or https");
    }
    let host = url.host_str().filter(|h| !h.is_empty());
    let Some(host) = host else {
        bail!("server URL has no host");
    };
    if !url.username().is_empty() || url.password().is_some() {
        bail!("server URL must not contain credentials");
    }
    if url.query().is_some() || url.fragment().is_some() {
        bail!("server URL must not contain a query or fragment");
    }
    let mut normalized = format!("{}://{}", url.scheme(), host.to_ascii_lowercase());
    if let Some(port) = url.port() {
        normalized.push_str(&format!(":{port}"));
    }
    let path = url.path().trim_end_matches('/');
    normalized.push_str(path);
    Ok(normalized)
}

/// Accepts what the user pastes after browser SSO: either the full
/// `furumi://auth/callback?code=furu_mx_...` link (copied from the
/// "Open Furumi" button) or the bare `furu_mx_...` code.
pub fn extract_sso_code(input: &str) -> Result<String> {
    let input = input.trim();
    if input.is_empty() {
        bail!("paste the link or code first");
    }
    if input.starts_with("furu_mx_") {
        return Ok(input.to_string());
    }
    if let Ok(url) = reqwest::Url::parse(input) {
        if let Some((_, error)) = url.query_pairs().find(|(k, _)| k == "error") {
            bail!("SSO failed: {error}");
        }
        if let Some((_, code)) = url.query_pairs().find(|(k, _)| k == "code") {
            return Ok(code.into_owned());
        }
    }
    bail!("no furu_mx_ code found in the pasted text");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_adds_https_and_strips_slash() {
        assert_eq!(
            normalize_base_url(" Music.Hexor.cy/ ").unwrap(),
            "https://music.hexor.cy"
        );
    }

    #[test]
    fn normalize_keeps_port_and_path() {
        assert_eq!(
            normalize_base_url("http://localhost:8000/furumi/").unwrap(),
            "http://localhost:8000/furumi"
        );
    }

    #[test]
    fn normalize_rejects_bad_urls() {
        assert!(normalize_base_url("").is_err());
        assert!(normalize_base_url("ftp://x").is_err());
        assert!(normalize_base_url("https://user:pw@host").is_err());
        assert!(normalize_base_url("https://host?x=1").is_err());
    }

    #[test]
    fn sso_code_from_deep_link() {
        let code = extract_sso_code("furumi://auth/callback?code=furu_mx_abc123").unwrap();
        assert_eq!(code, "furu_mx_abc123");
    }

    #[test]
    fn sso_code_bare() {
        assert_eq!(extract_sso_code(" furu_mx_x ").unwrap(), "furu_mx_x");
    }

    #[test]
    fn sso_error_is_reported() {
        let err = extract_sso_code("furumi://auth/callback?error=provider_denied")
            .unwrap_err()
            .to_string();
        assert!(err.contains("provider_denied"));
    }

    #[test]
    fn expiry_uses_skew() {
        let session = AuthSession {
            server_base_url: "https://x".into(),
            user: User {
                id: 1,
                name: "n".into(),
                role: "user".into(),
            },
            access_token: "a".into(),
            refresh_token: "r".into(),
            token_type: "Bearer".into(),
            expires_at_epoch_seconds: now_epoch_seconds() + 30,
        };
        assert!(session.access_token_expired());
    }
}
