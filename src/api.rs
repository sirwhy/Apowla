use anyhow::{Context, Result};
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, COOKIE, ORIGIN, REFERER, USER_AGENT};
use reqwest::{redirect, Client, StatusCode};
use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::config::Config;

/// Name of the session cookie set by https://api.rpow3.com after a successful
/// magic-link verification. The server reads this exact cookie name on every
/// authenticated request (`rpow_session`).
pub const SESSION_COOKIE_NAME: &str = "rpow_session";

#[derive(Debug, Deserialize)]
pub struct Challenge {
    pub challenge_id: String,
    pub nonce_prefix: String,
    pub difficulty_bits: u32,
}

#[derive(Debug, Serialize)]
struct MintRequest<'a> {
    challenge_id: &'a str,
    solution_nonce: &'a str,
}

#[derive(Debug, Deserialize)]
pub struct MintResponse {
    pub token: Token,
}

#[derive(Debug, Deserialize)]
pub struct Token {
    pub id: String,
}

#[derive(Debug, Deserialize)]
pub struct Me {
    pub email: Option<String>,
    pub balance: Option<u64>,
    pub minted: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    error: Option<String>,
    message: Option<String>,
}

#[derive(thiserror::Error, Debug)]
pub enum ApiCallError {
    #[error("authentication required (401): {0}")]
    Unauthorized(String),
    #[error("forbidden (403): {0}")]
    Forbidden(String),
    #[error("rate-limited (429)")]
    RateLimited,
    #[error("server error ({status}): {message}")]
    Server { status: u16, message: String },
    #[error("client error ({status}): {message}")]
    Client { status: u16, message: String },
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),
    #[error("invalid response body: {0}")]
    Body(String),
}

#[derive(Clone)]
pub struct ApiClient {
    http: Client,
    base: String,
}

impl ApiClient {
    pub fn new(cfg: &Config) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(
            COOKIE,
            HeaderValue::from_str(&cfg.cookie).context("invalid RPOW_COOKIE value")?,
        );
        headers.insert(
            ORIGIN,
            HeaderValue::from_str(&cfg.origin).context("invalid RPOW_ORIGIN value")?,
        );
        let referer = format!("{}/", cfg.origin.trim_end_matches('/'));
        headers.insert(
            REFERER,
            HeaderValue::from_str(&referer).context("invalid referer derived from RPOW_ORIGIN")?,
        );
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        headers.insert(
            USER_AGENT,
            HeaderValue::from_str(&cfg.user_agent).context("invalid RPOW_USER_AGENT")?,
        );

        let http = Client::builder()
            .default_headers(headers)
            .pool_idle_timeout(Some(Duration::from_secs(60)))
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(15))
            .https_only(true)
            .user_agent(cfg.user_agent.clone())
            .build()
            .context("building reqwest client")?;

        Ok(Self {
            http,
            base: cfg.api_base.clone(),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    async fn handle<T: for<'de> Deserialize<'de>>(
        &self,
        resp: reqwest::Response,
    ) -> Result<T, ApiCallError> {
        let status = resp.status();
        if status.is_success() {
            return resp
                .json::<T>()
                .await
                .map_err(|e| ApiCallError::Body(e.to_string()));
        }
        let body_text = resp.text().await.unwrap_or_default();
        let parsed: Option<ApiError> = serde_json::from_str(&body_text).ok();
        let message = parsed
            .as_ref()
            .and_then(|p| p.message.clone().or_else(|| p.error.clone()))
            .unwrap_or_else(|| body_text.clone());
        Err(match status {
            StatusCode::UNAUTHORIZED => ApiCallError::Unauthorized(message),
            StatusCode::FORBIDDEN => ApiCallError::Forbidden(message),
            StatusCode::TOO_MANY_REQUESTS => ApiCallError::RateLimited,
            s if s.is_server_error() => ApiCallError::Server {
                status: s.as_u16(),
                message,
            },
            s => ApiCallError::Client {
                status: s.as_u16(),
                message,
            },
        })
    }

    pub async fn me(&self) -> Result<Me, ApiCallError> {
        let resp = self.http.get(self.url("/me")).send().await?;
        self.handle(resp).await
    }

    pub async fn challenge(&self) -> Result<Challenge, ApiCallError> {
        let resp = self.http.post(self.url("/challenge")).send().await?;
        self.handle(resp).await
    }

    pub async fn mint(
        &self,
        challenge_id: &str,
        solution_nonce: &str,
    ) -> Result<MintResponse, ApiCallError> {
        let body = MintRequest {
            challenge_id,
            solution_nonce,
        };
        let resp = self
            .http
            .post(self.url("/mint"))
            .json(&body)
            .send()
            .await?;
        self.handle(resp).await
    }
}

/// Build a no-cookie HTTP client suitable for the unauthenticated half of the
/// login flow (POST /auth/request and GET /auth/verify). The verify endpoint
/// returns 302 with a Set-Cookie header — we deliberately do **not** follow
/// the redirect so we can inspect that header.
pub fn build_login_client(origin: &str, user_agent: &str) -> Result<Client> {
    let mut headers = HeaderMap::new();
    headers.insert(
        ORIGIN,
        HeaderValue::from_str(origin).context("invalid login origin")?,
    );
    let referer = format!("{}/", origin.trim_end_matches('/'));
    headers.insert(
        REFERER,
        HeaderValue::from_str(&referer).context("invalid login referer")?,
    );
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    headers.insert(
        USER_AGENT,
        HeaderValue::from_str(user_agent).context("invalid login user-agent")?,
    );

    Client::builder()
        .default_headers(headers)
        .redirect(redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(15))
        .https_only(true)
        .user_agent(user_agent.to_string())
        .build()
        .context("building login HTTP client")
}

#[derive(Debug, Serialize)]
struct AuthRequestBody<'a> {
    email: &'a str,
}

#[derive(Debug, Deserialize)]
pub struct AuthRequestResponse {
    pub ok: bool,
    #[serde(default)]
    #[allow(dead_code)]
    pub cooldown_seconds: Option<u64>,
}

/// Request a magic link to be emailed to the given address.
pub async fn auth_request(
    http: &Client,
    api_base: &str,
    email: &str,
) -> Result<AuthRequestResponse> {
    let resp = http
        .post(format!("{}/auth/request", api_base))
        .json(&AuthRequestBody { email })
        .send()
        .await
        .context("POST /auth/request: network error")?;
    let status = resp.status();
    let body_text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!(
            "auth/request failed: HTTP {} body={}",
            status.as_u16(),
            body_text
        );
    }
    serde_json::from_str(&body_text)
        .with_context(|| format!("parsing auth/request response body: {body_text}"))
}

/// Follow a magic-link verify URL once and extract the `rpow_session` cookie
/// from the resulting Set-Cookie header. The verify endpoint returns 302 on
/// success; we capture the cookie before the redirect.
pub async fn verify_magic_link(http: &Client, verify_url: &str) -> Result<String> {
    let resp = http
        .get(verify_url)
        .send()
        .await
        .context("GET /auth/verify: network error")?;
    let status = resp.status();
    if !(status.is_redirection() || status.is_success()) {
        let body_text = resp.text().await.unwrap_or_default();
        anyhow::bail!(
            "auth/verify failed: HTTP {} body={}",
            status.as_u16(),
            body_text
        );
    }
    let mut session_cookie: Option<String> = None;
    for header in resp.headers().get_all(reqwest::header::SET_COOKIE).iter() {
        let value = header.to_str().unwrap_or("");
        // Set-Cookie: name=value; Path=/; HttpOnly; Secure; ...
        let pair = value.split(';').next().unwrap_or("").trim();
        if let Some((name, val)) = pair.split_once('=') {
            if name.trim() == SESSION_COOKIE_NAME {
                session_cookie = Some(format!("{}={}", SESSION_COOKIE_NAME, val.trim()));
                break;
            }
        }
    }
    session_cookie.ok_or_else(|| {
        anyhow::anyhow!(
            "verify endpoint returned no '{SESSION_COOKIE_NAME}' cookie. The magic link may be invalid, expired, or already used."
        )
    })
}


