use async_trait::async_trait;
use axum::{
    Json,
    body::Body,
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures_util::StreamExt;
use http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    convert::Infallible,
    fs,
    path::PathBuf,
    pin::Pin,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::{
    anthropic::{
        error::json_error,
        schema::{CountTokensResponse, MessagesRequest},
    },
    monitor::{MonitorHandle, usage_from_anthropic_sse},
    provider::{
        CliHandlers, Generation, GenerationBody, Provider, ProviderError, ProviderErrorKind,
        RequestContext,
    },
    providers::{
        codex::translate::{
            IncompleteResponsePolicy, live_stream::LiveStreamTranslator as ResponsesTranslator,
        },
        grok::translate::stream::SseDecoder,
        kimi::count_tokens,
        opencode::{chat, responses},
    },
    traffic::TrafficCapture,
};

const CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
const DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const COPILOT_TOKEN_URL: &str = "https://api.github.com/copilot_internal/v2/token";
const COPILOT_BASE_URL: &str = "https://api.githubcopilot.com";
const PREFIX: &str = "github-copilot:";
const COPILOT_PREFIX: &str = "copilot:";
const FALLBACK_MODELS: &[&str] = &[
    "gpt-5.4",
    "gpt-5.4-mini",
    "gpt-5.5",
    "gpt-5.6-luna",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "claude-sonnet-4.6",
    "claude-opus-4.7",
    "gemini-3.5-pro",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredAuth {
    github_token: String,
    #[serde(default)]
    copilot_token: Option<String>,
    #[serde(default)]
    copilot_expires_at: Option<u64>,
    #[serde(default)]
    source: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DeviceCode {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: Option<u64>,
}
#[derive(Debug, Deserialize)]
struct OAuthReply {
    access_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}
#[derive(Debug, Deserialize)]
struct CopilotToken {
    token: String,
    expires_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WireApi {
    Chat,
    Responses,
}

impl WireApi {
    fn for_model(model: &str) -> Self {
        let id = model.to_ascii_lowercase();
        if id.starts_with("gpt-5") || id.starts_with("gpt-6") || id.contains("codex") {
            Self::Responses
        } else {
            Self::Chat
        }
    }
    fn path(self) -> &'static str {
        match self {
            Self::Chat => "/chat/completions",
            Self::Responses => "/v1/responses",
        }
    }
}

pub struct GithubCopilotProvider;
impl GithubCopilotProvider {
    pub fn new() -> Self {
        Self
    }
}
impl Default for GithubCopilotProvider {
    fn default() -> Self {
        Self::new()
    }
}

pub fn advertised_models() -> Vec<String> {
    let mut models = Vec::new();
    for m in FALLBACK_MODELS {
        models.push(format!("{COPILOT_PREFIX}{m}"));
        models.push(format!("{PREFIX}{m}"));
    }
    models
}

#[async_trait]
impl Provider for GithubCopilotProvider {
    fn name(&self) -> &'static str {
        "copilot"
    }
    fn supported_models(&self) -> Vec<String> {
        advertised_models()
    }
    fn cli(&self) -> &'static dyn CliHandlers {
        &CLI
    }

    async fn handle_messages(&self, body: MessagesRequest, ctx: RequestContext) -> Response {
        if body.stream {
            return match self.generate_anthropic_stream(body, ctx).await {
                Ok(g) => sse_response(g.body),
                Err(e) => error_response(e),
            };
        }
        let requested = body
            .model
            .clone()
            .unwrap_or_else(|| format!("{PREFIX}gpt-5.4"));
        let model = normalize_model(&requested);
        let wire = WireApi::for_model(&model);
        let response = match send_upstream(&body, &ctx, &model, wire).await {
            Ok(r) => r,
            Err(e) => return error_response(e),
        };
        let bytes = match response.bytes().await {
            Ok(b) => b,
            Err(e) => return json_error(StatusCode::BAD_GATEWAY, "api_error", e.to_string()),
        };
        let id = format!("msg_{}", uuid::Uuid::new_v4().simple());
        let translated = match wire {
            WireApi::Chat => chat::accumulate_response(&bytes, &id, &requested),
            WireApi::Responses => responses::accumulate_response(&bytes, &id, &requested),
        };
        match translated {
            Ok(value) => {
                if let Some(m) = ctx.monitor.as_ref() {
                    m.usage_updated(
                        &ctx.req_id,
                        value
                            .pointer("/usage/input_tokens")
                            .and_then(|v| v.as_u64()),
                        value
                            .pointer("/usage/output_tokens")
                            .and_then(|v| v.as_u64()),
                    );
                }
                (StatusCode::OK, Json(value)).into_response()
            }
            Err(e) => json_error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                format!("Copilot response translation failed: {e}"),
            ),
        }
    }

    async fn handle_count_tokens(&self, body: MessagesRequest, ctx: RequestContext) -> Response {
        let tokens = count_tokens::count_tokens(&body);
        if let Some(m) = ctx.monitor.as_ref() {
            m.usage_updated(&ctx.req_id, Some(tokens), None);
        }
        (
            StatusCode::OK,
            Json(CountTokensResponse {
                input_tokens: tokens,
            }),
        )
            .into_response()
    }

    async fn generate_anthropic_stream(
        &self,
        mut body: MessagesRequest,
        ctx: RequestContext,
    ) -> Result<Generation, ProviderError> {
        body.stream = true;
        let requested = body
            .model
            .clone()
            .unwrap_or_else(|| format!("{PREFIX}gpt-5.4"));
        let model = normalize_model(&requested);
        let wire = WireApi::for_model(&model);
        let estimated_input_tokens = count_tokens::count_tokens(&body);
        if let Some(m) = ctx.monitor.as_ref() {
            m.usage_updated(&ctx.req_id, Some(estimated_input_tokens), None);
        }
        let response = send_upstream(&body, &ctx, &model, wire).await?;
        let id = format!("msg_{}", uuid::Uuid::new_v4().simple());
        let upstream = Box::pin(response.bytes_stream());
        let body = match wire {
            WireApi::Chat => chat_stream(
                upstream,
                id,
                requested,
                estimated_input_tokens,
                ctx.monitor.clone(),
                ctx.req_id.clone(),
                ctx.traffic.clone(),
            ),
            WireApi::Responses => responses_stream(
                upstream,
                id,
                requested,
                estimated_input_tokens,
                ctx.monitor.clone(),
                ctx.req_id.clone(),
                ctx.traffic.clone(),
            ),
        };
        Ok(Generation {
            body: GenerationBody::LiveSse(body),
            resolved_model: model,
        })
    }
}

type Upstream = Pin<Box<dyn futures_core::Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;

async fn send_upstream(
    body: &MessagesRequest,
    ctx: &RequestContext,
    model: &str,
    wire: WireApi,
) -> Result<reqwest::Response, ProviderError> {
    if let Some(m) = ctx.monitor.as_ref() {
        m.model_resolved(&ctx.req_id, model);
        m.upstream_started(&ctx.req_id);
    }
    let payload = match wire {
        WireApi::Chat => {
            serde_json::to_value(chat::prepare_request(body, model).map_err(invalid_request)?)
                .map_err(|e| invalid_request(e.to_string()))?
        }
        WireApi::Responses => responses::prepare_request(body, model, ctx.session_id.clone())
            .map_err(invalid_request)?,
    };
    if let Some(t) = ctx.traffic.as_ref() {
        t.write_json("020-upstream-request", &payload);
    }
    let token = ensure_copilot_token().await.map_err(auth_error)?;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .build()
        .map_err(api_error)?;
    let request_id = uuid::Uuid::new_v4().to_string();
    let response = client
        .post(format!("{COPILOT_BASE_URL}{}", wire.path()))
        .header(http::header::AUTHORIZATION, format!("Bearer {token}"))
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(http::header::ACCEPT, "text/event-stream")
        .header("Editor-Version", editor_version())
        .header("Editor-Plugin-Version", plugin_version())
        .header("Copilot-Integration-Id", "vscode-chat")
        .header("OpenAI-Intent", "conversation-panel")
        .header("X-Interaction-Type", "conversation-panel")
        .header("X-GitHub-Api-Version", api_version())
        .header("openai-organization", "github-copilot")
        .header("X-Request-Id", &request_id)
        .header("X-Agent-Task-Id", &request_id)
        .header(http::header::USER_AGENT, user_agent())
        .json(&payload)
        .send()
        .await
        .map_err(|e| {
            ProviderError::new(
                StatusCode::BAD_GATEWAY,
                ProviderErrorKind::Api,
                format!("Copilot request failed: {e}"),
            )
        })?;
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let retry_after = response
        .headers()
        .get(http::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let text = response.text().await.unwrap_or_default();
    let kind = match status {
        StatusCode::UNAUTHORIZED => ProviderErrorKind::Authentication,
        StatusCode::FORBIDDEN => ProviderErrorKind::Permission,
        StatusCode::TOO_MANY_REQUESTS => ProviderErrorKind::RateLimit,
        s if s.is_client_error() => ProviderErrorKind::InvalidRequest,
        _ => ProviderErrorKind::Api,
    };
    let mut error = ProviderError::new(
        status,
        kind,
        if text.is_empty() {
            format!("Copilot upstream returned {status}")
        } else {
            text
        },
    );
    error.retry_after = retry_after;
    Err(error)
}

fn count_sse_events(bytes: &[u8]) -> u64 {
    String::from_utf8_lossy(bytes).matches("event:").count() as u64
}

fn chat_stream(
    upstream: Upstream,
    id: String,
    model: String,
    estimated_input_tokens: u64,
    monitor: Option<MonitorHandle>,
    req_id: String,
    traffic: Option<Arc<TrafficCapture>>,
) -> Body {
    struct State {
        upstream: Upstream,
        translator: chat::LiveStreamTranslator,
        monitor: Option<MonitorHandle>,
        req_id: String,
        bytes: u64,
        chunks: u64,
        done: bool,
        traffic: Option<Arc<TrafficCapture>>,
    }
    let state = State {
        upstream,
        translator: chat::LiveStreamTranslator::with_estimated_input_tokens(
            id,
            model,
            estimated_input_tokens,
        ),
        monitor,
        req_id,
        bytes: 0,
        chunks: 0,
        done: false,
        traffic,
    };
    Body::from_stream(futures_util::stream::unfold(state, |mut s| async move {
        if s.done {
            return None;
        }
        loop {
            match s.upstream.next().await {
                Some(Ok(chunk)) => {
                    if let Some(t) = s.traffic.as_ref() {
                        t.write_bytes("032-upstream-response-body.sse", &chunk);
                    }
                    if s.bytes == 0
                        && let Some(m) = s.monitor.as_ref()
                    {
                        m.generation_started(&s.req_id);
                    }
                    s.bytes = s.bytes.saturating_add(chunk.len() as u64);
                    s.chunks = s.chunks.saturating_add(1);
                    match s.translator.push(&chunk) {
                        Ok(out) if !out.is_empty() => {
                            let (input_tokens, output_tokens) = usage_from_anthropic_sse(&out);
                            if let Some(m) = s.monitor.as_ref() {
                                m.stream_progress(
                                    &s.req_id,
                                    out.len() as u64,
                                    count_sse_events(&out),
                                    input_tokens,
                                    output_tokens,
                                );
                            }
                            return Some((Ok::<Bytes, Infallible>(Bytes::from(out)), s));
                        }
                        Ok(_) => continue,
                        Err(e) => {
                            s.done = true;
                            return Some((
                                Ok(Bytes::from(chat::stream_error(&format!(
                                    "Copilot stream translation failed: {e}"
                                )))),
                                s,
                            ));
                        }
                    }
                }
                Some(Err(e)) => {
                    s.done = true;
                    return Some((
                        Ok(Bytes::from(chat::stream_error(&format!(
                            "Copilot stream failed: {e}"
                        )))),
                        s,
                    ));
                }
                None => {
                    s.done = true;
                    let out = s.translator.finish().unwrap_or_else(|e| {
                        chat::stream_error(&format!("Copilot stream ended unexpectedly: {e}"))
                    });
                    if !out.is_empty() {
                        let (input_tokens, output_tokens) = usage_from_anthropic_sse(&out);
                        if let Some(m) = s.monitor.as_ref() {
                            m.stream_progress(
                                &s.req_id,
                                out.len() as u64,
                                count_sse_events(&out),
                                input_tokens,
                                output_tokens,
                            );
                        }
                    }
                    return (!out.is_empty()).then(|| (Ok(Bytes::from(out)), s));
                }
            }
        }
    }))
}

fn responses_stream(
    upstream: Upstream,
    id: String,
    model: String,
    estimated_input_tokens: u64,
    monitor: Option<MonitorHandle>,
    req_id: String,
    traffic: Option<Arc<TrafficCapture>>,
) -> Body {
    struct State {
        upstream: Upstream,
        decoder: SseDecoder,
        translator: ResponsesTranslator,
        monitor: Option<MonitorHandle>,
        req_id: String,
        bytes: u64,
        chunks: u64,
        done: bool,
        traffic: Option<Arc<TrafficCapture>>,
    }
    let state = State {
        upstream,
        decoder: SseDecoder::default(),
        translator: ResponsesTranslator::with_estimated_input_tokens(
            id,
            model,
            estimated_input_tokens,
        )
        .with_incomplete_response_policy(IncompleteResponsePolicy::AllowMaxOutputTokens),
        monitor,
        req_id,
        bytes: 0,
        chunks: 0,
        done: false,
        traffic,
    };
    Body::from_stream(futures_util::stream::unfold(state, |mut s| async move {
        if s.done {
            return None;
        }
        loop {
            match s.upstream.next().await {
                Some(Ok(chunk)) => {
                    if let Some(t) = s.traffic.as_ref() {
                        t.write_bytes("032-upstream-response-body.sse", &chunk);
                    }
                    if s.bytes == 0
                        && let Some(m) = s.monitor.as_ref()
                    {
                        m.generation_started(&s.req_id);
                    }
                    s.bytes = s.bytes.saturating_add(chunk.len() as u64);
                    s.chunks = s.chunks.saturating_add(1);
                    let events = match s.decoder.push(&chunk) {
                        Ok(v) => v,
                        Err(e) => {
                            s.done = true;
                            let out = s.translator.error_chunk(
                                &format!("Copilot Responses SSE decode failed: {e}"),
                                "api_error",
                                s.traffic.as_deref(),
                            );
                            return Some((Ok::<Bytes, Infallible>(Bytes::from(out)), s));
                        }
                    };
                    let mut out = Vec::new();
                    for event in events {
                        let data = event.data.trim();
                        if data.is_empty() || data == "[DONE]" {
                            continue;
                        }
                        match serde_json::from_str::<Value>(data) {
                            Ok(value) => match s.translator.accept(&value, s.traffic.as_deref()) {
                                Ok(bytes) => out.extend(bytes),
                                Err(e) => {
                                    out.extend(s.translator.error_chunk(
                                        &e,
                                        "api_error",
                                        s.traffic.as_deref(),
                                    ));
                                    s.done = true;
                                    break;
                                }
                            },
                            Err(e) => {
                                out.extend(s.translator.error_chunk(
                                    &format!("Malformed Copilot Responses event: {e}"),
                                    "api_error",
                                    s.traffic.as_deref(),
                                ));
                                s.done = true;
                                break;
                            }
                        }
                    }
                    if s.translator.is_finished() {
                        s.done = true;
                    }
                    if !out.is_empty() {
                        let (input_tokens, output_tokens) = usage_from_anthropic_sse(&out);
                        if let Some(m) = s.monitor.as_ref() {
                            m.stream_progress(
                                &s.req_id,
                                out.len() as u64,
                                count_sse_events(&out),
                                input_tokens,
                                output_tokens,
                            );
                        }
                        return Some((Ok(Bytes::from(out)), s));
                    }
                    if s.done {
                        return None;
                    }
                }
                Some(Err(e)) => {
                    s.done = true;
                    let out = s.translator.error_chunk(
                        &format!("Copilot Responses stream failed: {e}"),
                        "api_error",
                        s.traffic.as_deref(),
                    );
                    return Some((Ok(Bytes::from(out)), s));
                }
                None => {
                    s.done = true;
                    if s.decoder.finish().is_err() || !s.translator.is_finished() {
                        let out = s.translator.error_chunk(
                            "Copilot Responses stream ended before completion",
                            "api_error",
                            s.traffic.as_deref(),
                        );
                        if !out.is_empty() {
                            let (input_tokens, output_tokens) = usage_from_anthropic_sse(&out);
                            if let Some(m) = s.monitor.as_ref() {
                                m.stream_progress(
                                    &s.req_id,
                                    out.len() as u64,
                                    count_sse_events(&out),
                                    input_tokens,
                                    output_tokens,
                                );
                            }
                        }
                        return (!out.is_empty()).then(|| (Ok(Bytes::from(out)), s));
                    }
                    return None;
                }
            }
        }
    }))
}

fn sse_response(body: GenerationBody) -> Response {
    let body = match body {
        GenerationBody::BufferedSse(v) => Body::from(v),
        GenerationBody::LiveSse(v) => v,
    };
    (
        [
            (http::header::CONTENT_TYPE, "text/event-stream"),
            (http::header::CACHE_CONTROL, "no-cache"),
            (http::header::CONNECTION, "keep-alive"),
        ],
        body,
    )
        .into_response()
}
fn invalid_request(e: impl std::fmt::Display) -> ProviderError {
    ProviderError::new(
        StatusCode::BAD_REQUEST,
        ProviderErrorKind::InvalidRequest,
        e.to_string(),
    )
}
fn auth_error(e: impl std::fmt::Display) -> ProviderError {
    ProviderError::new(
        StatusCode::UNAUTHORIZED,
        ProviderErrorKind::Authentication,
        e.to_string(),
    )
}
fn api_error(e: impl std::fmt::Display) -> ProviderError {
    ProviderError::new(
        StatusCode::BAD_GATEWAY,
        ProviderErrorKind::Api,
        e.to_string(),
    )
}
fn error_response(error: ProviderError) -> Response {
    let response = json_error(error.status, error.error_type(), error.message);
    if let Some(v) = error.retry_after {
        ([(http::header::RETRY_AFTER, v)], response).into_response()
    } else {
        response
    }
}

fn normalize_model(model: &str) -> String {
    let id = model
        .strip_prefix(PREFIX)
        .or_else(|| model.strip_prefix(COPILOT_PREFIX))
        .unwrap_or(model);
    if id.starts_with("gpt-") {
        id.strip_suffix("-fast").unwrap_or(id).to_string()
    } else {
        id.to_string()
    }
}
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn normalize_expiry(v: u64) -> u64 {
    if v > 100_000_000_000 { v / 1000 } else { v }
}
fn client_id() -> String {
    std::env::var("CCP_COPILOT_CLIENT_ID").unwrap_or_else(|_| CLIENT_ID.into())
}
fn editor_version() -> String {
    std::env::var("CCP_COPILOT_VSCODE_VERSION").unwrap_or_else(|_| "vscode/1.107.0".into())
}
fn plugin_version() -> String {
    std::env::var("CCP_COPILOT_PLUGIN_VERSION").unwrap_or_else(|_| "copilot-chat/0.35.0".into())
}
fn api_version() -> String {
    std::env::var("CCP_COPILOT_API_VERSION").unwrap_or_else(|_| "2025-04-01".into())
}
fn user_agent() -> String {
    format!(
        "GitHubCopilotChat/{}",
        plugin_version().trim_start_matches("copilot-chat/")
    )
}
fn auth_path() -> PathBuf {
    let copilot_file = crate::paths::provider_auth_file("copilot");
    if copilot_file.exists() {
        return copilot_file;
    }
    let legacy_file = crate::paths::provider_auth_file("github-copilot");
    if legacy_file.exists() {
        return legacy_file;
    }
    copilot_file
}

fn load_auth() -> anyhow::Result<StoredAuth> {
    let text = fs::read_to_string(auth_path()).map_err(|_| anyhow::anyhow!("Not authenticated; run `claude-code-proxy github-copilot auth login` or `github-copilot copy vscode|opencode`"))?;
    Ok(serde_json::from_str(&text)?)
}
fn save_auth(auth: &StoredAuth) -> anyhow::Result<()> {
    let path = auth_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_vec_pretty(auth)?)?;
    Ok(())
}

async fn ensure_copilot_token() -> anyhow::Result<String> {
    let mut auth = load_auth()?;
    if let (Some(token), Some(exp)) = (&auth.copilot_token, auth.copilot_expires_at)
        && normalize_expiry(exp) > now_secs() + 60
    {
        return Ok(token.clone());
    }
    let response = reqwest::Client::new()
        .get(COPILOT_TOKEN_URL)
        .header(
            http::header::AUTHORIZATION,
            format!("token {}", auth.github_token),
        )
        .header(http::header::ACCEPT, "application/json")
        .header("Editor-Version", editor_version())
        .header("Editor-Plugin-Version", plugin_version())
        .header("X-GitHub-Api-Version", api_version())
        .header("X-VSCode-User-Agent-Library-Version", "electron-fetch")
        .header(http::header::USER_AGENT, user_agent())
        .send()
        .await?;
    if !response.status().is_success() {
        anyhow::bail!("Copilot token exchange failed: HTTP {}", response.status());
    }
    let token: CopilotToken = response.json().await?;
    auth.copilot_token = Some(token.token.clone());
    auth.copilot_expires_at = Some(token.expires_at);
    save_auth(&auth)?;
    Ok(token.token)
}
fn ensure_copilot_token_blocking() -> anyhow::Result<String> {
    let mut auth = load_auth()?;
    if let (Some(token), Some(exp)) = (&auth.copilot_token, auth.copilot_expires_at)
        && normalize_expiry(exp) > now_secs() + 60
    {
        return Ok(token.clone());
    }
    let response = reqwest::blocking::Client::new()
        .get(COPILOT_TOKEN_URL)
        .header(
            http::header::AUTHORIZATION,
            format!("token {}", auth.github_token),
        )
        .header(http::header::ACCEPT, "application/json")
        .header("Editor-Version", editor_version())
        .header("Editor-Plugin-Version", plugin_version())
        .header("X-GitHub-Api-Version", api_version())
        .header("X-VSCode-User-Agent-Library-Version", "electron-fetch")
        .header(http::header::USER_AGENT, user_agent())
        .send()?;
    if !response.status().is_success() {
        anyhow::bail!("Copilot token exchange failed: HTTP {}", response.status());
    }
    let token: CopilotToken = response.json()?;
    auth.copilot_token = Some(token.token.clone());
    auth.copilot_expires_at = Some(token.expires_at);
    save_auth(&auth)?;
    Ok(token.token)
}

pub fn discover_models() -> anyhow::Result<Vec<String>> {
    let token = ensure_copilot_token_blocking()?;
    let response = reqwest::blocking::Client::new()
        .get(format!("{COPILOT_BASE_URL}/models"))
        .header(http::header::AUTHORIZATION, format!("Bearer {token}"))
        .header(http::header::ACCEPT, "application/json")
        .header("Editor-Version", editor_version())
        .header("Editor-Plugin-Version", plugin_version())
        .header("Copilot-Integration-Id", "vscode-chat")
        .header("X-GitHub-Api-Version", api_version())
        .header(http::header::USER_AGENT, user_agent())
        .send()?;
    if !response.status().is_success() {
        anyhow::bail!("Copilot model discovery failed: HTTP {}", response.status());
    }
    let root: Value = response.json()?;
    let list = root
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| root.as_array())
        .ok_or_else(|| anyhow::anyhow!("Copilot /models returned unexpected payload"))?;
    let mut out: Vec<String> = list
        .iter()
        .filter_map(|v| v.get("id").and_then(Value::as_str))
        .map(|id| format!("{PREFIX}{}", normalize_model(id)))
        .collect();
    out.sort();
    out.dedup();
    Ok(out)
}

pub fn import_from(source: &str) -> anyhow::Result<()> {
    let auth = match source {
        "vscode" => import_vscode()?,
        "opencode" => import_opencode()?,
        _ => anyhow::bail!("unsupported source: {source}; use vscode or opencode"),
    };
    save_auth(&auth)?;
    println!("Copied GitHub Copilot credentials from {source}");
    println!("Auth saved in {}", auth_path().display());
    Ok(())
}

fn import_vscode() -> anyhow::Result<StoredAuth> {
    for name in ["COPILOT_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(token) = std::env::var(name)
            && valid_github_token(&token)
        {
            return Ok(StoredAuth {
                github_token: token,
                copilot_token: None,
                copilot_expires_at: None,
                source: Some(format!("vscode:{name}")),
            });
        }
    }
    for path in vscode_candidates() {
        if !path.exists() {
            continue;
        }
        let root: Value = serde_json::from_str(&fs::read_to_string(&path)?)?;
        if let Some(token) = find_token(&root) {
            return Ok(StoredAuth {
                github_token: token,
                copilot_token: None,
                copilot_expires_at: None,
                source: Some(format!("vscode:{}", path.display())),
            });
        }
    }
    anyhow::bail!("no reusable VS Code/Copilot GitHub token found")
}

fn import_opencode() -> anyhow::Result<StoredAuth> {
    let path = opencode_auth_path();
    let root: Value = serde_json::from_str(&fs::read_to_string(&path)?)?;
    for key in ["github-copilot", "github-copilot-enterprise"] {
        let Some(v) = root.get(key) else { continue };
        let github = v
            .get("refresh")
            .or_else(|| v.get("refreshToken"))
            .and_then(Value::as_str);
        let access = v
            .get("access")
            .or_else(|| v.get("accessToken"))
            .and_then(Value::as_str);
        let expires = v
            .get("expires")
            .or_else(|| v.get("expiresAt"))
            .and_then(Value::as_u64)
            .map(normalize_expiry);
        if let Some(github_token) = github.filter(|v| valid_github_token(v)) {
            return Ok(StoredAuth {
                github_token: github_token.into(),
                copilot_token: access.map(str::to_string),
                copilot_expires_at: expires,
                source: Some(format!("opencode:{key}")),
            });
        }
    }
    anyhow::bail!("no github-copilot credentials found in {}", path.display())
}

fn valid_github_token(s: &str) -> bool {
    ["gho_", "ghu_", "github_pat_"]
        .iter()
        .any(|p| s.starts_with(p))
}
fn find_token(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if valid_github_token(s) => Some(s.clone()),
        Value::Array(a) => a.iter().find_map(find_token),
        Value::Object(o) => o.values().find_map(find_token),
        _ => None,
    }
}
fn home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}
fn vscode_candidates() -> Vec<PathBuf> {
    let root = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"));
    vec![
        root.join("github-copilot/hosts.json"),
        root.join("github-copilot/apps.json"),
    ]
}
fn opencode_auth_path() -> PathBuf {
    if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(home)
            .join("opencode/auth.json")
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".local/share"))
            .join("opencode/auth.json")
    }
}

fn form_body(pairs: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs.iter().copied())
        .finish()
}

pub fn run_device_login() -> anyhow::Result<()> {
    let client = reqwest::blocking::Client::new();
    let client_id = client_id();
    let response = client
        .post(DEVICE_CODE_URL)
        .header(http::header::ACCEPT, "application/json")
        .header(
            http::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(form_body(&[
            ("client_id", client_id.as_str()),
            ("scope", "read:user"),
        ]))
        .send()?
        .error_for_status()?;
    let device: DeviceCode = response.json()?;
    println!("Open: {}", device.verification_uri);
    println!("Code: {}", device.user_code);
    let interval = device.interval.unwrap_or(5).max(1);
    let deadline = now_secs() + device.expires_in;
    while now_secs() < deadline {
        std::thread::sleep(Duration::from_secs(interval));
        let response = client
            .post(ACCESS_TOKEN_URL)
            .header(http::header::ACCEPT, "application/json")
            .header(
                http::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(form_body(&[
                ("client_id", client_id.as_str()),
                ("device_code", device.device_code.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ]))
            .send()?;
        let reply: OAuthReply = response.json()?;
        if let Some(token) = reply.access_token {
            save_auth(&StoredAuth {
                github_token: token,
                copilot_token: None,
                copilot_expires_at: None,
                source: Some("device-flow".into()),
            })?;
            println!("Authentication complete");
            return Ok(());
        }
        match reply.error.as_deref() {
            Some("authorization_pending") => continue,
            Some("slow_down") => {
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }
            Some(e) => anyhow::bail!(
                "GitHub device login failed: {}",
                reply.error_description.as_deref().unwrap_or(e)
            ),
            None => continue,
        }
    }
    anyhow::bail!("GitHub device login expired")
}

struct CopilotCli;
impl CliHandlers for CopilotCli {
    fn login(&self) -> anyhow::Result<()> {
        run_device_login()
    }
    fn device(&self) -> anyhow::Result<()> {
        run_device_login()
    }
    fn status(&self) -> anyhow::Result<()> {
        let auth = load_auth()?;
        println!("Authenticated: true");
        println!("Auth path: {}", auth_path().display());
        if let Some(source) = auth.source {
            println!("Source: {source}");
        }
        Ok(())
    }
    fn logout(&self) -> anyhow::Result<()> {
        let path = auth_path();
        if path.exists() {
            fs::remove_file(path)?;
        }
        println!("Logged out");
        Ok(())
    }
}
static CLI: CopilotCli = CopilotCli;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn gpt_fast_is_alias_only() {
        assert_eq!(
            normalize_model("github-copilot:gpt-5.6-sol-fast"),
            "gpt-5.6-sol"
        );
        assert_eq!(normalize_model("copilot:gpt-5.6-sol-fast"), "gpt-5.6-sol");
        assert!(advertised_models().iter().all(|m| !m.ends_with("-fast")));
    }
    #[test]
    fn gpt_uses_responses_api() {
        assert_eq!(WireApi::for_model("gpt-5.6-sol"), WireApi::Responses);
        assert_eq!(WireApi::for_model("claude-sonnet-4.6"), WireApi::Chat);
    }
    #[test]
    fn millisecond_expiry_is_normalized() {
        assert_eq!(normalize_expiry(1_900_000_000_000), 1_900_000_000);
    }
    #[test]
    fn classic_pat_is_not_reused() {
        assert!(!valid_github_token("ghp_legacy"));
        assert!(valid_github_token("gho_token"));
    }
    #[test]
    fn nested_token_is_found() {
        let v = json!({"github.com":{"oauth_token":"gho_abc"}});
        assert_eq!(find_token(&v).as_deref(), Some("gho_abc"));
    }
}
