use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::Mutex;

use super::auth::{self, AuthSession};
use super::models::{
    ApiErrorBody, ArtistDetail, ArtistsPage, LikesResponse, LoginResponse, MeResponse,
    PlaylistCard, PlaylistDetail, ReleaseDetail, SearchResults, TokensResponse, TrackItem,
};

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{0}")]
    Server(String),
    /// Refresh token rejected or expired — the user must sign in again.
    #[error("session expired, please sign in again")]
    SessionExpired,
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),
    #[error("{0}")]
    Other(#[from] anyhow::Error),
}

pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(format!(
            "furumi-tui/{} ({})",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS
        ))
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("reqwest client config is static")
}

pub fn device_name() -> String {
    format!("furumi-tui ({})", std::env::consts::OS)
}

#[derive(Serialize)]
struct PasswordLoginRequest<'a> {
    username: &'a str,
    password: &'a str,
    device_name: String,
}

#[derive(Serialize)]
struct SsoExchangeRequest<'a> {
    code: &'a str,
    device_name: String,
}

#[derive(Serialize)]
struct RefreshRequest<'a> {
    refresh_token: &'a str,
}

#[derive(Serialize)]
struct LogoutRequest<'a> {
    refresh_token: &'a str,
}

pub async fn login_password(
    http: &reqwest::Client,
    base_url: &str,
    username: &str,
    password: &str,
) -> Result<AuthSession, ApiError> {
    let response = http
        .post(format!("{base_url}/api/auth/password"))
        .json(&PasswordLoginRequest {
            username,
            password,
            device_name: device_name(),
        })
        .send()
        .await?;
    let login: LoginResponse = parse_response(response).await?;
    Ok(AuthSession::new(base_url.to_string(), login.user, login.tokens))
}

pub async fn login_sso_exchange(
    http: &reqwest::Client,
    base_url: &str,
    code: &str,
) -> Result<AuthSession, ApiError> {
    let response = http
        .post(format!("{base_url}/api/auth/sso/exchange"))
        .json(&SsoExchangeRequest {
            code,
            device_name: device_name(),
        })
        .send()
        .await?;
    let login: LoginResponse = parse_response(response).await?;
    Ok(AuthSession::new(base_url.to_string(), login.user, login.tokens))
}

/// Browser entry point for SSO. redirect_uri is either our loopback
/// listener (`http://127.0.0.1:{port}/callback`) or the `furumi://` deep
/// link as a manual-paste fallback.
pub fn sso_start_url(base_url: &str, redirect_uri: &str) -> String {
    let mut url = reqwest::Url::parse(&format!("{base_url}/auth/mobile/oidc/start"))
        .expect("base_url is pre-validated");
    url.query_pairs_mut()
        .append_pair("redirect_uri", redirect_uri);
    url.to_string()
}

async fn refresh_tokens(
    http: &reqwest::Client,
    base_url: &str,
    refresh_token: &str,
) -> Result<TokensResponse, ApiError> {
    let response = http
        .post(format!("{base_url}/api/auth/refresh"))
        .json(&RefreshRequest { refresh_token })
        .send()
        .await?;
    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err(ApiError::SessionExpired);
    }
    parse_response(response).await
}

/// Mirrors the backend's PlaybackStateDto.
#[derive(Debug, Serialize)]
pub struct PlaybackStateBody {
    pub current_track_id: Option<i64>,
    pub position_ms: i32,
    pub queue: Vec<i64>,
    pub queue_position: i32,
    pub shuffle: bool,
    pub repeat_mode: String,
    pub volume: f64,
}

/// Percent-encode a query-string value.
fn url_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

async fn parse_response<T: DeserializeOwned>(response: reqwest::Response) -> Result<T, ApiError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response.json().await?);
    }
    let message = match response.json::<ApiErrorBody>().await {
        Ok(body) => body.error,
        Err(_) => format!("server returned {status}"),
    };
    Err(ApiError::Server(message))
}

/// Authenticated API client. Owns the session; refreshes the access token
/// proactively (60s skew) and once more on 401, persisting rotated tokens.
/// The session mutex makes concurrent refreshes single-flight.
pub struct ApiClient {
    http: reqwest::Client,
    base_url: String,
    session: Mutex<AuthSession>,
}

impl ApiClient {
    pub fn new(http: reqwest::Client, session: AuthSession) -> Self {
        Self {
            http,
            base_url: session.server_base_url.clone(),
            session: Mutex::new(session),
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub async fn me(&self) -> Result<MeResponse, ApiError> {
        self.get_json("/api/player/me").await
    }

    pub async fn artists(&self, page: i64, limit: i64) -> Result<ArtistsPage, ApiError> {
        self.get_json(&format!("/api/player/artists?page={page}&limit={limit}"))
            .await
    }

    pub async fn artist(&self, id: i64) -> Result<ArtistDetail, ApiError> {
        self.get_json(&format!("/api/player/artists/{id}")).await
    }

    pub async fn release(&self, id: i64) -> Result<ReleaseDetail, ApiError> {
        self.get_json(&format!("/api/player/releases/{id}")).await
    }

    pub async fn search(&self, query: &str, limit: i64) -> Result<SearchResults, ApiError> {
        self.get_json(&format!(
            "/api/player/search?q={}&limit={limit}",
            url_encode(query)
        ))
        .await
    }

    /// Open an audio stream for playback: background download backed by a
    /// temp file, exposing blocking Read+Seek for the decoder; seeking into
    /// not-yet-downloaded ranges uses HTTP Range requests.
    ///
    /// The download client carries the bearer token valid at start; on very
    /// long tracks a Range request after token expiry (15 min) can fail —
    /// acceptable for now, a refreshing middleware can replace this later.
    pub async fn open_stream(
        &self,
        path: &str,
    ) -> Result<(crate::player::TrackReader, Option<u64>), ApiError> {
        use stream_download::Settings;
        use stream_download::http::HttpStream;
        use stream_download::source::SourceStream as _;
        use stream_download::storage::temp::TempStorageProvider;

        let token = self.fresh_access_token().await?;
        let mut headers = reqwest::header::HeaderMap::new();
        let value = format!("Bearer {token}")
            .parse()
            .map_err(|_| ApiError::Server("invalid token header".to_string()))?;
        headers.insert(reqwest::header::AUTHORIZATION, value);
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .map_err(ApiError::Network)?;

        let url = format!("{}{path}", self.base_url)
            .parse()
            .map_err(|err| ApiError::Server(format!("bad stream url: {err}")))?;
        let stream = HttpStream::new(client, url)
            .await
            .map_err(|err| ApiError::Server(format!("stream open failed: {err}")))?;
        let byte_len = stream.content_length();
        let reader = stream_download::StreamDownload::from_stream(
            stream,
            TempStorageProvider::new(),
            Settings::default(),
        )
        .await
        .map_err(|err| ApiError::Server(format!("stream start failed: {err}")))?;
        Ok((reader, byte_len))
    }

    /// Raw bytes (cover art, artist images) from a server-relative path.
    pub async fn get_bytes(&self, path: &str) -> Result<Vec<u8>, ApiError> {
        let url = format!("{}{path}", self.base_url);
        let response = self
            .send_authed(&url, |client, url, token| client.get(url).bearer_auth(token))
            .await?;
        let status = response.status();
        if !status.is_success() {
            return Err(ApiError::Server(format!("server returned {status}")));
        }
        Ok(response.bytes().await?.to_vec())
    }

    pub async fn playlists(&self) -> Result<Vec<PlaylistCard>, ApiError> {
        self.get_json("/api/player/playlists").await
    }

    pub async fn playlist(&self, id: i64) -> Result<PlaylistDetail, ApiError> {
        self.get_json(&format!("/api/player/playlists/{id}")).await
    }

    pub async fn likes(&self) -> Result<Vec<i64>, ApiError> {
        let response: LikesResponse = self.get_json("/api/player/likes").await?;
        Ok(response.track_ids)
    }

    pub async fn toggle_like(&self, track_id: i64) -> Result<bool, ApiError> {
        #[derive(serde::Deserialize)]
        struct Body {
            liked: bool,
        }
        let body: Body = self
            .post_json(&format!("/api/player/likes/toggle/{track_id}"), &())
            .await?;
        Ok(body.liked)
    }

    #[allow(dead_code, reason = "device-sync state restore needs id→track resolution")]
    pub async fn tracks_by_ids(&self, track_ids: &[i64]) -> Result<Vec<TrackItem>, ApiError> {
        #[derive(Serialize)]
        struct Body<'a> {
            track_ids: &'a [i64],
        }
        self.post_json("/api/player/tracks-by-ids", &Body { track_ids })
            .await
    }

    /// Report a finished/aborted listen to the play history.
    pub async fn report_history(
        &self,
        track_id: i64,
        started_at: Option<i64>,
        listened_seconds: i32,
    ) -> Result<(), ApiError> {
        #[derive(Serialize)]
        struct Body {
            track_id: i64,
            started_at: Option<i64>,
            listened_seconds: i32,
        }
        let _: serde_json::Value = self
            .post_json(
                "/api/player/history",
                &Body {
                    track_id,
                    started_at,
                    listened_seconds,
                },
            )
            .await?;
        Ok(())
    }

    /// Persist playback state server-side (used for cross-device restore).
    pub async fn push_state(&self, state: &PlaybackStateBody) -> Result<(), ApiError> {
        let _: serde_json::Value = self.put_json("/api/player/state", state).await?;
        Ok(())
    }

    /// Revoke this device's session server-side. Best effort: local
    /// credentials are deleted regardless of the outcome.
    pub async fn logout(&self) -> Result<bool, ApiError> {
        let (access_token, refresh_token) = {
            let session = self.session.lock().await;
            (session.access_token.clone(), session.refresh_token.clone())
        };
        let response = self
            .http
            .post(format!("{}/api/auth/logout", self.base_url))
            .bearer_auth(access_token)
            .json(&LogoutRequest {
                refresh_token: &refresh_token,
            })
            .send()
            .await?;

        #[derive(serde::Deserialize)]
        struct LogoutResponse {
            revoked: bool,
        }
        let body: LogoutResponse = parse_response(response).await?;
        Ok(body.revoked)
    }

    pub async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T, ApiError> {
        self.json_request::<(), T>(reqwest::Method::GET, path, None)
            .await
    }

    pub async fn post_json<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, ApiError> {
        self.json_request(reqwest::Method::POST, path, Some(body))
            .await
    }

    pub async fn put_json<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, ApiError> {
        self.json_request(reqwest::Method::PUT, path, Some(body))
            .await
    }

    async fn json_request<B: Serialize, T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<T, ApiError> {
        let url = format!("{}{path}", self.base_url);
        let response = self
            .send_authed(&url, |client, url, token| {
                let mut request = client.request(method.clone(), url).bearer_auth(token);
                if let Some(body) = body {
                    request = request.json(body);
                }
                request
            })
            .await;
        let response = match response {
            Ok(response) => response,
            Err(err) => {
                tracing::warn!(%err, %method, path, "api request failed");
                return Err(err);
            }
        };
        let status = response.status();
        let result = parse_response(response).await;
        if let Err(err) = &result {
            tracing::warn!(%err, %status, %method, path, "api response error");
        } else {
            tracing::debug!(%status, %method, path, "api ok");
        }
        result
    }

    /// Send a request with a fresh bearer token; on 401, refresh once and
    /// retry. `build` is called per attempt because RequestBuilder is not
    /// reusable after send.
    async fn send_authed<F>(&self, url: &str, build: F) -> Result<reqwest::Response, ApiError>
    where
        F: Fn(&reqwest::Client, &str, &str) -> reqwest::RequestBuilder,
    {
        let token = self.fresh_access_token().await?;
        let response = build(&self.http, url, &token).send().await?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            let token = self.refresh_after_rejection(&token).await?;
            return Ok(build(&self.http, url, &token).send().await?);
        }
        Ok(response)
    }

    async fn fresh_access_token(&self) -> Result<String, ApiError> {
        let mut session = self.session.lock().await;
        if session.access_token_expired() {
            self.refresh_locked(&mut session).await?;
        }
        Ok(session.access_token.clone())
    }

    /// A 401 with a token another task already rotated just retries with the
    /// current token; otherwise this task performs the refresh itself.
    async fn refresh_after_rejection(&self, rejected_token: &str) -> Result<String, ApiError> {
        let mut session = self.session.lock().await;
        if session.access_token != rejected_token {
            return Ok(session.access_token.clone());
        }
        self.refresh_locked(&mut session).await?;
        Ok(session.access_token.clone())
    }

    async fn refresh_locked(&self, session: &mut AuthSession) -> Result<(), ApiError> {
        let result = refresh_tokens(&self.http, &self.base_url, &session.refresh_token).await;
        match result {
            Ok(tokens) => {
                session.apply_tokens(tokens);
                if let Err(err) = auth::save_session(session) {
                    tracing::warn!(%err, "failed to persist rotated tokens");
                }
                tracing::debug!("access token refreshed");
                Ok(())
            }
            Err(ApiError::SessionExpired) => {
                auth::delete_session();
                Err(ApiError::SessionExpired)
            }
            Err(err) => Err(err),
        }
    }
}
