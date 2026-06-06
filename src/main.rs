// Allow warnings from generated macro code
#![allow(clippy::derive_partial_eq_without_eq)]
#![allow(clippy::needless_for_each)]
#![allow(clippy::needless_raw_string_hashes)]

use axum::{
    Router,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use graphql_parser::query::{
    Definition as QueryDefinition, OperationDefinition, Selection, parse_query,
};
use hmac::{Hmac, Mac};
use rclap::config;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::sync::Arc;
use std::time::Instant;
use std::{fs, path::Path};
use tokio::sync::RwLock;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;
use tracing::{debug, error, info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
#[cfg(feature = "swagger-ui")]
use utoipa::OpenApi;
#[cfg(feature = "swagger-ui")]
use utoipa_swagger_ui::SwaggerUi;
use uuid::Uuid;

mod agent;
mod answer_synthesis;
mod capabilities;
mod domain_config;
mod entity_linker;
mod error;
mod history;
mod intermediate_representation;
mod introspection;
mod mcp;
mod metric_formula;
mod openai;
mod pipeline;
#[cfg(test)]
mod plan_v2_tests;
mod planner;
mod planner_cache;
mod policy_guard;
mod progress;
mod prompt_examples;
mod prompts;
mod provider;
mod query_executor;
mod query_repair;
mod schema_registry;
mod service;
mod sls;
mod sls_derive;
mod transformations;

use crate::agent::{LlmAgent, create_answer_agent, create_ir_agent, execute_graphql};
use crate::history::{HistoryEntry, QueryHistory};
use crate::openai::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, Choice, ChoiceChunk, Delta,
    GraphQLToolRequest, GraphQLToolResponse, Message, Usage,
};
use crate::pipeline::{run_ir_pipeline, run_ir_pipeline_with_progress};
use crate::planner_cache::PlannerPromptCache;
use crate::progress::PipelineProgressEvent;
use crate::schema_registry::SchemaRegistry;
use crate::sls::{Sls, load_sls_merged};

type HmacSha256 = Hmac<Sha256>;

// Bundled development fallback schema used only when no runtime schema source is available.
const BUNDLED_FALLBACK_SCHEMA: &str = include_str!("../schemas/consumer_schema.graphql");
const SESSION_COOKIE_NAME: &str = "zephyr_session";
const ADMIN_ROLE: &str = "admin";
const ANONYMOUS_ROLE: &str = "anonymous";
const GRAPHQL_INTROSPECTION_QUERY: &str = r#"query IntrospectSchema {
  __schema {
    queryType { name }
    types {
      kind
      name
      fields(includeDeprecated: true) {
        name
        args {
          name
          type {
            ...TypeRef
          }
        }
        type {
          ...TypeRef
        }
      }
      inputFields {
        name
        type {
          ...TypeRef
        }
      }
      enumValues(includeDeprecated: true) {
        name
      }
    }
  }
}

fragment TypeRef on __Type {
  kind
  name
  ofType {
    kind
    name
    ofType {
      kind
      name
      ofType {
        kind
        name
        ofType {
          kind
          name
          ofType {
            kind
            name
            ofType {
              kind
              name
            }
          }
        }
      }
    }
  }
}"#;

#[config("config.toml")]
pub struct Config;

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) config: Config,
    pub(crate) client: Client,
    pub(crate) schema_registry: Arc<RwLock<Arc<SchemaRegistry>>>,
    pub(crate) schema_meta: Arc<RwLock<SchemaMeta>>,
    pub(crate) history: Arc<RwLock<QueryHistory>>,
    pub(crate) history_path: String,
    pub(crate) sls: Option<Sls>,
    pub(crate) cached_ir_agent: LlmAgent,
    pub(crate) cached_answer_agent: LlmAgent,
    pub(crate) planner_cache: Arc<RwLock<PlannerPromptCache>>,
}

#[derive(Clone, Debug)]
pub(crate) struct SchemaMeta {
    pub(crate) source: schema_registry::SchemaSource,
    pub(crate) loaded_at: DateTime<Utc>,
    pub(crate) cache_path: Option<String>,
    pub(crate) cache_stale: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SchemaCacheEnvelope {
    fetched_at: String,
    response: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct HistorySearchRequest {
    query: String,
}

#[derive(Debug, Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

#[derive(Debug, Serialize)]
struct SessionResponse {
    authenticated: bool,
    role: String,
    admin_configured: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct AdminSession {
    username: String,
    role: String,
    expires_at: i64,
    nonce: String,
}

const HISTORY_FILE_PATH: &str = "history.json";

fn admin_configured(config: &Config) -> bool {
    admin_config_ready(&config.auth.admin_password, &config.auth.session_secret)
}

fn admin_config_ready(admin_password: &str, session_secret: &str) -> bool {
    !admin_password.trim().is_empty() && !session_secret.trim().is_empty()
}

fn admin_password_without_secret(config: &Config) -> bool {
    !config.auth.admin_password.trim().is_empty() && config.auth.session_secret.trim().is_empty()
}

fn session_ttl_seconds(config: &Config) -> i64 {
    config
        .auth
        .session_ttl_hours
        .parse::<i64>()
        .ok()
        .filter(|hours| *hours > 0)
        .unwrap_or(8)
        * 60
        * 60
}

fn session_cookie_secure(config: &Config) -> bool {
    config
        .auth
        .session_cookie_secure
        .parse::<bool>()
        .unwrap_or(false)
}

fn sign_session_payload(secret: &str, payload_b64: &str) -> Option<String> {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).ok()?;
    mac.update(payload_b64.as_bytes());
    Some(URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes()))
}

fn create_session_token(
    username: &str,
    secret: &str,
    expires_at: i64,
    nonce: &str,
) -> Option<String> {
    let session = AdminSession {
        username: username.to_string(),
        role: ADMIN_ROLE.to_string(),
        expires_at,
        nonce: nonce.to_string(),
    };
    let payload = serde_json::to_vec(&session).ok()?;
    let payload_b64 = URL_SAFE_NO_PAD.encode(payload);
    let signature_b64 = sign_session_payload(secret, &payload_b64)?;
    Some(format!("{payload_b64}.{signature_b64}"))
}

fn validate_session_token(secret: &str, token: &str, now: i64) -> Option<AdminSession> {
    let (payload_b64, signature_b64) = token.split_once('.')?;
    let signature = URL_SAFE_NO_PAD.decode(signature_b64).ok()?;
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).ok()?;
    mac.update(payload_b64.as_bytes());
    mac.verify_slice(&signature).ok()?;
    let payload = URL_SAFE_NO_PAD.decode(payload_b64).ok()?;
    let session: AdminSession = serde_json::from_slice(&payload).ok()?;
    if session.role != ADMIN_ROLE || session.expires_at <= now {
        return None;
    }
    Some(session)
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    for value in headers.get_all(header::COOKIE) {
        let Ok(raw) = value.to_str() else {
            continue;
        };
        for part in raw.split(';') {
            let trimmed = part.trim();
            let Some((cookie_name, cookie_value)) = trimmed.split_once('=') else {
                continue;
            };
            if cookie_name == name {
                return Some(cookie_value.to_string());
            }
        }
    }
    None
}

fn admin_session_from_headers(config: &Config, headers: &HeaderMap) -> Option<AdminSession> {
    if !admin_configured(config) {
        return None;
    }
    let token = cookie_value(headers, SESSION_COOKIE_NAME)?;
    validate_session_token(&config.auth.session_secret, &token, Utc::now().timestamp())
}

fn request_is_admin(config: &Config, headers: &HeaderMap) -> bool {
    admin_session_from_headers(config, headers).is_some()
}

fn session_response(config: &Config, headers: &HeaderMap) -> SessionResponse {
    let authenticated = request_is_admin(config, headers);
    SessionResponse {
        authenticated,
        role: if authenticated {
            ADMIN_ROLE
        } else {
            ANONYMOUS_ROLE
        }
        .to_string(),
        admin_configured: admin_configured(config),
    }
}

fn session_cookie_header(config: &Config, token: &str, max_age_seconds: i64) -> String {
    let mut cookie = format!(
        "{SESSION_COOKIE_NAME}={token}; HttpOnly; SameSite=Lax; Path=/; Max-Age={max_age_seconds}"
    );
    if session_cookie_secure(config) {
        cookie.push_str("; Secure");
    }
    cookie
}

fn cleared_session_cookie_header(config: &Config) -> String {
    session_cookie_header(config, "", 0)
}

fn admin_required_response() -> Response {
    (
        StatusCode::FORBIDDEN,
        axum::Json(serde_json::json!({
            "error": "Admin login required for debug features."
        })),
    )
        .into_response()
}

fn admin_guard(config: &Config, headers: &HeaderMap) -> Option<Response> {
    if request_is_admin(config, headers) {
        None
    } else {
        Some(admin_required_response())
    }
}

fn debug_request_guard(is_admin: bool, debug_output: bool, execute: bool) -> Option<Response> {
    if !debug_request_allowed(is_admin, debug_output, execute) {
        Some(admin_required_response())
    } else {
        None
    }
}

fn debug_request_allowed(is_admin: bool, debug_output: bool, execute: bool) -> bool {
    (!debug_output && execute) || is_admin
}

fn with_set_cookie(mut response: Response, cookie: String) -> Response {
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
}

async fn admin_static_file(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    path: &str,
    content_type: &'static str,
) -> Response {
    if let Some(response) = admin_guard(&state.config, headers) {
        return response;
    }
    match tokio::fs::read(path).await {
        Ok(content) => {
            let mut response = (StatusCode::OK, content).into_response();
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
            response
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn fetch_live_introspection(
    client: &Client,
    config: &Config,
) -> Result<serde_json::Value, String> {
    let bearer_token = if config.graph.bearer_token.is_empty() {
        None
    } else {
        Some(config.graph.bearer_token.as_str())
    };
    let api_key_header = if config.graph.api_key_header.is_empty() {
        None
    } else {
        Some(config.graph.api_key_header.as_str())
    };
    let api_key = if config.graph.api_key.is_empty() {
        None
    } else {
        Some(config.graph.api_key.as_str())
    };

    let response = execute_graphql(
        client,
        &config.graph.graph_endpoint,
        bearer_token,
        api_key_header,
        api_key,
        GRAPHQL_INTROSPECTION_QUERY,
        &serde_json::json!({}),
    )
    .await
    .map_err(|e| e.to_string())?;

    if let Some(errors) = response.get("errors").and_then(|value| value.as_array())
        && !errors.is_empty()
    {
        let messages = errors
            .iter()
            .filter_map(|error| {
                error
                    .get("message")
                    .and_then(|value| value.as_str())
                    .map(str::to_string)
            })
            .collect::<Vec<_>>();
        return Err(messages.join("; "));
    }

    Ok(response)
}

fn write_schema_cache(cache_path: &str, response: &serde_json::Value, fetched_at: DateTime<Utc>) {
    if cache_path.is_empty() {
        return;
    }
    let envelope = SchemaCacheEnvelope {
        fetched_at: fetched_at.to_rfc3339(),
        response: response.clone(),
    };
    if let Ok(serialized) = serde_json::to_string_pretty(&envelope) {
        if let Some(parent) = Path::new(cache_path).parent()
            && !parent.as_os_str().is_empty()
            && let Err(e) = fs::create_dir_all(parent)
        {
            warn!("Failed to create schema cache dir: {e}");
        }
        if let Err(e) = fs::write(cache_path, serialized) {
            warn!("Failed to write schema cache file: {e}");
        }
    }
}

fn load_history_from_disk(history_path: &str) -> QueryHistory {
    match fs::read_to_string(history_path) {
        Ok(raw) => match QueryHistory::from_json(&raw) {
            Ok(history) => {
                info!(
                    "Loaded query history from {} ({} entries).",
                    history_path,
                    history.len()
                );
                history
            }
            Err(e) => {
                warn!(
                    "Failed to parse history file {} as JSON; starting with empty history: {}",
                    history_path, e
                );
                QueryHistory::new(100)
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => QueryHistory::new(100),
        Err(e) => {
            warn!(
                "Failed to read history file {}; starting with empty history: {}",
                history_path, e
            );
            QueryHistory::new(100)
        }
    }
}

pub(crate) fn persist_history_to_disk(history_path: &str, history: &QueryHistory) {
    let Ok(serialized) = history.to_json_pretty() else {
        warn!("Failed to serialize query history for persistence.");
        return;
    };
    if let Some(parent) = Path::new(history_path).parent()
        && !parent.as_os_str().is_empty()
        && let Err(e) = fs::create_dir_all(parent)
    {
        warn!(
            "Failed to create history directory for {}: {}",
            history_path, e
        );
        return;
    }
    if let Err(e) = fs::write(history_path, serialized) {
        warn!("Failed to write history file {}: {}", history_path, e);
    }
}

#[cfg(feature = "swagger-ui")]
#[derive(OpenApi)]
#[openapi(
    paths(graphql_query, health, chat_completions),
    components(schemas(
        GraphQLToolRequest,
        GraphQLToolResponse,
        ChatCompletionRequest,
        ChatCompletionResponse,
        Message,
        Choice,
        Usage
    ))
)]
struct ApiDoc;

fn init_tracing(log_to_stderr: bool) {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "zephyr_agent=info,tower_http=debug".into());
    if log_to_stderr {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
            .init();
    } else {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(tracing_subscriber::fmt::layer())
            .init();
    }
}

async fn load_schema_registry(
    client: &Client,
    config: &Config,
    sls: Option<&Sls>,
) -> (SchemaRegistry, SchemaMeta) {
    let cache_path = config.schema.cache_path.trim().to_string();
    let schema_file_path = config.schema.file_path.trim().to_string();
    let cache_ttl_minutes = config.schema.cache_ttl_minutes.parse::<i64>().unwrap_or(0);
    let cache_ttl = if cache_ttl_minutes <= 0 {
        None
    } else {
        Some(cache_ttl_minutes)
    };

    let mut cached_envelope: Option<(SchemaCacheEnvelope, DateTime<Utc>)> = None;
    if !cache_path.is_empty()
        && let Ok(raw) = fs::read_to_string(&cache_path)
    {
        if let Ok(envelope) = serde_json::from_str::<SchemaCacheEnvelope>(&raw) {
            if let Ok(ts) = DateTime::parse_from_rfc3339(&envelope.fetched_at) {
                cached_envelope = Some((envelope, ts.with_timezone(&Utc)));
            } else {
                warn!("Schema cache timestamp is invalid; ignoring cached schema.");
            }
        } else {
            warn!("Schema cache file is invalid JSON; ignoring cached schema.");
        }
    }

    fn load_schema_registry_from_file(
        schema_file_path: &str,
        sls: Option<&Sls>,
    ) -> Result<SchemaRegistry, String> {
        let raw = fs::read_to_string(schema_file_path)
            .map_err(|e| format!("failed to read schema file {}: {}", schema_file_path, e))?;
        let trimmed = raw.trim();
        if trimmed.starts_with('{') {
            if let Ok(envelope) = serde_json::from_str::<SchemaCacheEnvelope>(trimmed) {
                return SchemaRegistry::from_introspection_response_with_source(
                    &envelope.response,
                    sls,
                    schema_registry::SchemaSource::LocalFile,
                )
                .map_err(|e| e.to_string());
            }
            let response: serde_json::Value = serde_json::from_str(trimmed).map_err(|e| {
                format!(
                    "failed to parse schema file {} as introspection JSON: {}",
                    schema_file_path, e
                )
            })?;
            return SchemaRegistry::from_introspection_response_with_source(
                &response,
                sls,
                schema_registry::SchemaSource::LocalFile,
            )
            .map_err(|e| e.to_string());
        }
        Ok(SchemaRegistry::with_sls_and_source(
            trimmed,
            sls,
            schema_registry::SchemaSource::LocalFile,
        ))
    }

    let now = Utc::now();
    if !schema_file_path.is_empty() {
        match load_schema_registry_from_file(&schema_file_path, sls) {
            Ok(registry) => {
                info!(
                    "Loaded schema registry from configured schema file {}.",
                    schema_file_path
                );
                return (
                    registry,
                    SchemaMeta {
                        source: schema_registry::SchemaSource::LocalFile,
                        loaded_at: now,
                        cache_path: if cache_path.is_empty() {
                            None
                        } else {
                            Some(cache_path.clone())
                        },
                        cache_stale: false,
                    },
                );
            }
            Err(e) => {
                warn!(
                    "Failed to load configured schema file {}; falling back to other sources: {}",
                    schema_file_path, e
                );
            }
        }
    }

    match fetch_live_introspection(client, config).await {
        Ok(response) => match SchemaRegistry::from_introspection_response(&response, sls) {
            Ok(registry) => {
                info!(
                    "Loaded schema registry from live introspection at {}",
                    config.graph.graph_endpoint
                );
                write_schema_cache(&cache_path, &response, now);
                return (
                    registry,
                    SchemaMeta {
                        source: schema_registry::SchemaSource::LiveIntrospection,
                        loaded_at: now,
                        cache_path: if cache_path.is_empty() {
                            None
                        } else {
                            Some(cache_path.clone())
                        },
                        cache_stale: false,
                    },
                );
            }
            Err(e) => {
                warn!(
                    "Failed to build schema registry from live introspection; falling back to static schema: {}",
                    e
                );
            }
        },
        Err(e) => {
            warn!(
                "Failed to fetch live schema introspection from {}; falling back to static schema: {}",
                config.graph.graph_endpoint, e
            );
        }
    }

    if let Some((envelope, fetched_at)) = cached_envelope {
        let cache_age_minutes = (now - fetched_at).num_minutes();
        let cache_stale = cache_ttl.is_some_and(|ttl| cache_age_minutes > ttl);
        if cache_stale {
            warn!(
                "Schema cache is stale (age {} min, ttl {:?} min); using cached schema anyway.",
                cache_age_minutes, cache_ttl
            );
        } else {
            info!(
                "Loaded schema registry from cached introspection (age {} min).",
                cache_age_minutes
            );
        }
        match SchemaRegistry::from_introspection_response_with_source(
            &envelope.response,
            sls,
            schema_registry::SchemaSource::CachedIntrospection,
        ) {
            Ok(registry) => {
                return (
                    registry,
                    SchemaMeta {
                        source: schema_registry::SchemaSource::CachedIntrospection,
                        loaded_at: fetched_at,
                        cache_path: if cache_path.is_empty() {
                            None
                        } else {
                            Some(cache_path.clone())
                        },
                        cache_stale,
                    },
                );
            }
            Err(e) => {
                warn!("Failed to build schema registry from cached introspection: {e}");
            }
        }
    }

    info!("Loaded schema registry from bundled fallback schema.");
    (
        SchemaRegistry::with_sls(BUNDLED_FALLBACK_SCHEMA, sls),
        SchemaMeta {
            source: schema_registry::SchemaSource::StaticFile,
            loaded_at: now,
            cache_path: if cache_path.is_empty() {
                None
            } else {
                Some(cache_path)
            },
            cache_stale: false,
        },
    )
}

fn refresh_interval_minutes(config: &Config) -> i64 {
    config
        .schema
        .refresh_interval_minutes
        .parse::<i64>()
        .unwrap_or(0)
}

async fn refresh_schema_registry_once(state: &Arc<AppState>) -> Result<(), String> {
    let schema_file_path = state.config.schema.file_path.trim().to_string();
    if !schema_file_path.is_empty() {
        let raw = fs::read_to_string(&schema_file_path)
            .map_err(|e| format!("failed to read schema file {}: {}", schema_file_path, e))?;
        let trimmed = raw.trim();
        let registry = if trimmed.starts_with('{') {
            let response =
                if let Ok(envelope) = serde_json::from_str::<SchemaCacheEnvelope>(trimmed) {
                    envelope.response
                } else {
                    serde_json::from_str::<serde_json::Value>(trimmed).map_err(|e| {
                        format!(
                            "failed to parse schema file {} as introspection JSON: {}",
                            schema_file_path, e
                        )
                    })?
                };
            SchemaRegistry::from_introspection_response_with_source(
                &response,
                state.sls.as_ref(),
                schema_registry::SchemaSource::LocalFile,
            )
            .map_err(|e| e.to_string())?
        } else {
            SchemaRegistry::with_sls_and_source(
                trimmed,
                state.sls.as_ref(),
                schema_registry::SchemaSource::LocalFile,
            )
        };
        let now = Utc::now();
        {
            let mut guard = state.schema_registry.write().await;
            *guard = Arc::new(registry);
        }
        {
            let mut meta = state.schema_meta.write().await;
            meta.source = schema_registry::SchemaSource::LocalFile;
            meta.loaded_at = now;
            meta.cache_stale = false;
        }
        state.planner_cache.write().await.clear();
        return Ok(());
    }

    let response = fetch_live_introspection(&state.client, &state.config).await?;
    let registry = SchemaRegistry::from_introspection_response_with_source(
        &response,
        state.sls.as_ref(),
        schema_registry::SchemaSource::LiveIntrospection,
    )
    .map_err(|e| e.to_string())?;
    let now = Utc::now();
    let cache_path = state.config.schema.cache_path.trim().to_string();
    write_schema_cache(&cache_path, &response, now);

    {
        let mut guard = state.schema_registry.write().await;
        *guard = Arc::new(registry);
    }
    {
        let mut meta = state.schema_meta.write().await;
        meta.source = schema_registry::SchemaSource::LiveIntrospection;
        meta.loaded_at = now;
        meta.cache_path = if cache_path.is_empty() {
            None
        } else {
            Some(cache_path)
        };
        meta.cache_stale = false;
    }
    state.planner_cache.write().await.clear();
    Ok(())
}

fn spawn_schema_refresh_loop(state: Arc<AppState>) {
    let refresh_minutes = refresh_interval_minutes(&state.config);
    if refresh_minutes <= 0 {
        info!("Schema refresh loop disabled (refresh_interval_minutes <= 0).");
        return;
    }
    let interval = std::time::Duration::from_secs((refresh_minutes as u64) * 60);
    tokio::spawn(async move {
        info!(
            "Schema refresh loop enabled (interval {} minute(s)).",
            refresh_minutes
        );
        loop {
            tokio::time::sleep(interval).await;
            match refresh_schema_registry_once(&state).await {
                Ok(()) => info!("Schema registry refreshed from live introspection."),
                Err(e) => warn!("Schema refresh failed; keeping existing registry: {e}"),
            }
        }
    });
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::parse();
    let mcp_enabled = mcp::mcp_enabled(&config);
    let http_enabled = mcp::http_enabled(&config);
    init_tracing(mcp_enabled && mcp::uses_stdio_transport(&config));
    if !mcp_enabled && !http_enabled {
        anyhow::bail!("both ZEPHYR_HTTP_ENABLED and ZEPHYR_MCP_ENABLED are false");
    }
    if mcp_enabled && !mcp::uses_stdio_transport(&config) {
        anyhow::bail!(
            "unsupported MCP_TRANSPORT='{}'; v1 supports MCP_TRANSPORT=stdio only",
            config.mcp_transport
        );
    }
    let client = Client::new();

    // Step 1: Load schema registry without SLS so we can derive a base semantic layer from it.
    let (bootstrap_registry, _bootstrap_meta) = load_schema_registry(&client, &config, None).await;

    // Step 2: Auto-derive SLS from schema registry and merge with sparse manual overrides.
    let sls = match load_sls_merged(&bootstrap_registry, "sls.yaml") {
        Ok(merged) => {
            info!(
                "Auto-derived and merged SLS: {} concepts (auto) + manual overrides, {} metrics",
                merged.concepts.len(),
                merged
                    .metrics
                    .as_ref()
                    .map_or(0, std::collections::HashMap::len)
            );
            Some(merged)
        }
        Err(e) => {
            error!("Failed to auto-derive merged SLS: {e}");
            None
        }
    };

    // Step 3: Rebuild schema registry with the merged SLS so SLS-backed field roles influence domain config.
    let (schema_registry, schema_meta) = load_schema_registry(&client, &config, sls.as_ref()).await;
    let cached_ir_agent = create_ir_agent(&config, "").await?;
    let cached_answer_agent = create_answer_agent(&config, "").await?;

    let host = config.server_host.clone();
    let port = config.server_port.parse::<u16>()?;
    let history_path = HISTORY_FILE_PATH.to_string();
    let history = load_history_from_disk(&history_path);
    if admin_password_without_secret(&config) {
        warn!("ADMIN_PASSWORD is set but SESSION_SECRET is empty; admin login is disabled.");
    }

    let state = Arc::new(AppState {
        config,
        client,
        schema_registry: Arc::new(RwLock::new(Arc::new(schema_registry))),
        schema_meta: Arc::new(RwLock::new(schema_meta)),
        history: Arc::new(RwLock::new(history)),
        history_path,
        sls,
        cached_ir_agent,
        cached_answer_agent,
        planner_cache: Arc::new(RwLock::new(PlannerPromptCache::default())),
    });

    spawn_schema_refresh_loop(state.clone());

    match (http_enabled, mcp_enabled) {
        (true, true) => {
            let http_state = state.clone();
            let mcp_state = state.clone();
            tokio::select! {
                result = run_http_server(http_state, host, port) => result?,
                result = mcp::run_stdio_server(mcp_state) => result?,
            }
        }
        (true, false) => run_http_server(state, host, port).await?,
        (false, true) => mcp::run_stdio_server(state).await?,
        (false, false) => unreachable!("runtime mode guard should reject all-disabled mode"),
    }

    Ok(())
}

async fn run_http_server(state: Arc<AppState>, host: String, port: u16) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/health", get(health))
        .route("/auth/session", get(auth_session))
        .route("/auth/login", post(auth_login))
        .route("/auth/logout", post(auth_logout))
        .route("/config", get(config_endpoint))
        .route("/history", get(history_entries).delete(clear_history))
        .route("/history/stats", get(history_stats))
        .route("/history/search", post(search_history))
        .route("/graphql/query", post(graphql_query))
        .route("/", get(demo_redesigned_html))
        .route("/demo.html", get(demo_redesigned_html))
        .route("/demo_redesigned.html", get(demo_redesigned_html))
        .route("/demo_charts_redesigned.js", get(demo_charts_redesigned_js))
        .route("/eval-dashboard.html", get(admin_eval_dashboard_html))
        .route("/eval_dashboard.js", get(admin_eval_dashboard_js))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/chat/completions/stream", post(chat_completions_stream))
        .route("/chat/completions", post(chat_completions))
        .route("/chat/completions/stream", post(chat_completions_stream))
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        .fallback_service(ServeDir::new("public").append_index_html_on_directories(true))
        .with_state(state);
    #[cfg(feature = "swagger-ui")]
    let app = app.merge(SwaggerUi::new("/swagger-ui").url("/openapi.json", ApiDoc::openapi()));

    let addr = format!("{host}:{port}");
    info!("Starting Zephyr Agent server at http://{}", addr);
    #[cfg(feature = "swagger-ui")]
    info!("Swagger UI available at http://{}/swagger-ui/", addr);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

/// Health check endpoint
#[utoipa::path(
    get,
    path = "/health",
    responses(
        (status = 200, description = "Service is healthy")
    )
)]
async fn health() -> impl IntoResponse {
    axum::Json(serde_json::json!({"status": "ok"}))
}

async fn auth_session(State(state): State<Arc<AppState>>, headers: HeaderMap) -> impl IntoResponse {
    axum::Json(session_response(&state.config, &headers))
}

async fn auth_login(
    State(state): State<Arc<AppState>>,
    axum::Json(req): axum::Json<LoginRequest>,
) -> Response {
    if !admin_configured(&state.config) {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({
                "error": "Admin login is not configured."
            })),
        )
            .into_response();
    }

    if req.username != state.config.auth.admin_username
        || req.password != state.config.auth.admin_password
    {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(serde_json::json!({
                "error": "Invalid username or password."
            })),
        )
            .into_response();
    }

    let now = Utc::now().timestamp();
    let ttl = session_ttl_seconds(&state.config);
    let Some(token) = create_session_token(
        &state.config.auth.admin_username,
        &state.config.auth.session_secret,
        now + ttl,
        &Uuid::new_v4().to_string(),
    ) else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({
                "error": "Could not create admin session."
            })),
        )
            .into_response();
    };

    let response = (
        StatusCode::OK,
        axum::Json(SessionResponse {
            authenticated: true,
            role: ADMIN_ROLE.to_string(),
            admin_configured: true,
        }),
    )
        .into_response();
    with_set_cookie(response, session_cookie_header(&state.config, &token, ttl))
}

async fn auth_logout(State(state): State<Arc<AppState>>) -> Response {
    let response = (
        StatusCode::OK,
        axum::Json(SessionResponse {
            authenticated: false,
            role: ANONYMOUS_ROLE.to_string(),
            admin_configured: admin_configured(&state.config),
        }),
    )
        .into_response();
    with_set_cookie(response, cleared_session_cookie_header(&state.config))
}

async fn public_static_file(path: &str, content_type: &'static str) -> Response {
    match tokio::fs::read(path).await {
        Ok(content) => {
            let mut response = (StatusCode::OK, content).into_response();
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
            response
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn demo_redesigned_html() -> Response {
    public_static_file("public/demo_redesigned.html", "text/html; charset=utf-8").await
}

async fn demo_charts_redesigned_js() -> Response {
    public_static_file(
        "public/demo_charts_redesigned.js",
        "application/javascript; charset=utf-8",
    )
    .await
}

async fn admin_eval_dashboard_html(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    admin_static_file(
        &state,
        &headers,
        "public/eval-dashboard.html",
        "text/html; charset=utf-8",
    )
    .await
}

async fn admin_eval_dashboard_js(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    admin_static_file(
        &state,
        &headers,
        "public/eval_dashboard.js",
        "application/javascript; charset=utf-8",
    )
    .await
}

/// Config inspection endpoint (debug)
#[utoipa::path(
    get,
    path = "/config",
    responses(
        (status = 200, description = "Effective configuration")
    )
)]
async fn config_endpoint(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Some(response) = admin_guard(&state.config, &headers) {
        return response;
    }
    let schema_meta = state.schema_meta.read().await;
    let schema_age_minutes = (Utc::now() - schema_meta.loaded_at).num_minutes();
    let provider_kind = provider::infer_provider_kind(&state.config, &state.config.model);
    let provider_prompt_cache = provider::prompt_cache_profile(provider_kind);
    axum::Json(serde_json::json!({
        "llm_provider": state.config.llm_provider,
        "model": state.config.model,
        "openai_model": state.config.openai_model,
        "anthropic_model": state.config.anthropic_model,
        "ollama_url": state.config.ollama_url,
        "provider_prompt_cache": provider_prompt_cache,
        "execute_enabled": state.config.execute_enabled,
        "direct_graphql_query_enabled": state.config.graph.direct_query_enabled,
        "sls_loaded": state.sls.is_some(),
        "schema_source": schema_meta.source.as_str(),
        "schema_loaded_at": schema_meta.loaded_at.to_rfc3339(),
        "schema_cache_path": schema_meta.cache_path.clone(),
        "schema_cache_stale": schema_meta.cache_stale,
        "schema_cache_age_minutes": schema_age_minutes,
        "schema_file_path": state.config.schema.file_path.clone()
    }))
    .into_response()
}

async fn history_entries(State(state): State<Arc<AppState>>) -> Response {
    let history = state.history.read().await;
    axum::Json(history.get_recent(100)).into_response()
}

async fn clear_history(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Some(response) = admin_guard(&state.config, &headers) {
        return response;
    }
    let mut history = state.history.write().await;
    history.clear();
    persist_history_to_disk(&state.history_path, &history);
    StatusCode::NO_CONTENT.into_response()
}

async fn history_stats(State(state): State<Arc<AppState>>) -> Response {
    let history = state.history.read().await;
    axum::Json(history.stats()).into_response()
}

async fn search_history(
    State(state): State<Arc<AppState>>,
    axum::Json(req): axum::Json<HistorySearchRequest>,
) -> Response {
    let history = state.history.read().await;
    axum::Json(history.search(&req.query)).into_response()
}

fn user_facing_error_message(debug_output: bool, err: &crate::error::PipelineError) -> String {
    if debug_output {
        err.to_string()
    } else {
        "I couldn’t complete that request with the available data and query path.".to_string()
    }
}

async fn record_history_entry(
    state: &Arc<AppState>,
    question: &str,
    answer: String,
    success: bool,
    error: Option<String>,
    execution_ms: u64,
) {
    let mut entry = HistoryEntry::new(question.to_string());
    entry.answer = Some(answer);
    entry.execution_ms = execution_ms;
    entry.success = success;
    entry.error = error;
    let mut history = state.history.write().await;
    history.add(entry);
    persist_history_to_disk(&state.history_path, &history);
}

enum StreamUpdate {
    Stage(PipelineProgressEvent),
    Answer(String),
    Error(String),
    Done(String),
}

fn stage_payload(
    event_id: &str,
    started_at: Instant,
    event: &PipelineProgressEvent,
) -> serde_json::Value {
    let mut payload = serde_json::to_value(event).unwrap_or_else(|_| {
        serde_json::json!({
            "stage": "unknown",
            "status": "running",
            "message": "Working."
        })
    });
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("id".to_string(), serde_json::json!(event_id));
        obj.insert(
            "elapsed_ms".to_string(),
            serde_json::json!(started_at.elapsed().as_millis()),
        );
    }
    payload
}

pub(crate) fn direct_graphql_query_enabled(config: &Config) -> bool {
    config
        .graph
        .direct_query_enabled
        .parse::<bool>()
        .unwrap_or(false)
}

fn reject_disallowed_direct_selection_set(
    selections: &[Selection<'_, String>],
) -> Result<(), String> {
    for selection in selections {
        match selection {
            Selection::Field(field) => {
                if field.name.starts_with("__") {
                    return Err(
                        "direct GraphQL execution does not allow introspection fields".to_string(),
                    );
                }
                reject_disallowed_direct_selection_set(&field.selection_set.items)?;
            }
            Selection::InlineFragment(_) => {
                return Err("direct GraphQL execution does not allow inline fragments".to_string());
            }
            Selection::FragmentSpread(_) => {
                return Err(
                    "direct GraphQL execution does not allow named fragment spreads".to_string(),
                );
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_direct_graphql_query(
    schema_registry: &SchemaRegistry,
    query: &str,
) -> Result<(), String> {
    let ast = parse_query::<String>(query).map_err(|e| format!("invalid GraphQL syntax: {e}"))?;
    let mut query_operation_count = 0usize;

    for definition in ast.definitions {
        match definition {
            QueryDefinition::Operation(OperationDefinition::Query(query_op)) => {
                query_operation_count += 1;
                if !query_op.variable_definitions.is_empty() {
                    return Err(
                        "direct GraphQL execution does not allow variables; inline literal arguments so the request can be fully validated"
                            .to_string(),
                    );
                }
                reject_disallowed_direct_selection_set(&query_op.selection_set.items)?;
            }
            QueryDefinition::Operation(OperationDefinition::SelectionSet(_)) => {
                return Err(
                    "direct GraphQL execution requires an explicit `query` operation; shorthand selection sets are not accepted"
                        .to_string(),
                );
            }
            QueryDefinition::Operation(OperationDefinition::Mutation(_)) => {
                return Err("direct GraphQL execution does not allow mutations".to_string());
            }
            QueryDefinition::Operation(OperationDefinition::Subscription(_)) => {
                return Err("direct GraphQL execution does not allow subscriptions".to_string());
            }
            QueryDefinition::Fragment(_) => {
                return Err(
                    "direct GraphQL execution does not allow named fragment definitions"
                        .to_string(),
                );
            }
        }
    }

    if query_operation_count == 0 {
        return Err("direct GraphQL execution requires one query operation".to_string());
    }
    if query_operation_count > 1 {
        return Err(
            "direct GraphQL execution accepts only one query operation per request".to_string(),
        );
    }

    schema_registry
        .validate_query(query)
        .map_err(|e| e.to_string())
}

pub(crate) fn direct_graphql_variables_are_empty(variables: &serde_json::Value) -> bool {
    variables.is_null() || variables.as_object().is_some_and(serde_json::Map::is_empty)
}

fn graphql_json_error(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        axum::Json(serde_json::json!({ "error": message.into() })),
    )
        .into_response()
}

#[cfg(test)]
mod direct_graphql_tests {
    use super::{
        BUNDLED_FALLBACK_SCHEMA, direct_graphql_variables_are_empty, validate_direct_graphql_query,
    };
    use crate::schema_registry::SchemaRegistry;

    fn registry() -> SchemaRegistry {
        SchemaRegistry::new(BUNDLED_FALLBACK_SCHEMA)
    }

    #[test]
    fn direct_graphql_validation_accepts_schema_valid_query() {
        let query = r#"query FarmLookup {
            queryOffshoreWindFarm(first: 1) {
                name
                shortName
            }
        }"#;

        assert!(validate_direct_graphql_query(&registry(), query).is_ok());
    }

    #[test]
    fn direct_graphql_validation_rejects_mutation() {
        let query = r#"mutation Bad {
            queryOffshoreWindFarm {
                name
            }
        }"#;

        let err = validate_direct_graphql_query(&registry(), query).expect_err("must reject");
        assert!(err.contains("mutations"), "unexpected error: {err}");
    }

    #[test]
    fn direct_graphql_validation_rejects_introspection() {
        let query = r#"query Introspection {
            __schema {
                queryType {
                    name
                }
            }
        }"#;

        let err = validate_direct_graphql_query(&registry(), query).expect_err("must reject");
        assert!(err.contains("introspection"), "unexpected error: {err}");
    }

    #[test]
    fn direct_graphql_validation_rejects_unvalidated_shapes() {
        let shorthand = r#"{
            queryOffshoreWindFarm {
                name
            }
        }"#;
        let err = validate_direct_graphql_query(&registry(), shorthand).expect_err("must reject");
        assert!(err.contains("explicit `query`"), "unexpected error: {err}");

        let fragment = r#"query FarmLookup {
            queryOffshoreWindFarm {
                ...FarmFields
            }
        }

        fragment FarmFields on OffshoreWindFarm {
            name
        }"#;
        let err = validate_direct_graphql_query(&registry(), fragment).expect_err("must reject");
        assert!(err.contains("fragment spreads"), "unexpected error: {err}");

        let inline_fragment = r#"query FarmLookup {
            queryOffshoreWindFarm {
                ... on OffshoreWindFarm {
                    name
                }
            }
        }"#;
        let err =
            validate_direct_graphql_query(&registry(), inline_fragment).expect_err("must reject");
        assert!(err.contains("inline fragments"), "unexpected error: {err}");
    }

    #[test]
    fn direct_graphql_validation_rejects_variables() {
        let query = r#"query FarmLookup($filter: OffshoreWindFarmFilter) {
            queryOffshoreWindFarm(filter: $filter) {
                name
            }
        }"#;

        let err = validate_direct_graphql_query(&registry(), query).expect_err("must reject");
        assert!(err.contains("variables"), "unexpected error: {err}");
        assert!(direct_graphql_variables_are_empty(&serde_json::Value::Null));
        assert!(direct_graphql_variables_are_empty(&serde_json::json!({})));
        assert!(!direct_graphql_variables_are_empty(
            &serde_json::json!({"filter": {"name": {"eq": "Wind Farm 1"}}})
        ));
    }
}

#[cfg(test)]
mod auth_tests {
    use super::{
        ADMIN_ROLE, admin_config_ready, create_session_token, debug_request_allowed,
        validate_session_token,
    };

    #[test]
    fn signed_session_validates_when_untouched() {
        let token = create_session_token("admin", "secret", 1_000, "nonce").expect("token");
        let session = validate_session_token("secret", &token, 999).expect("valid session");
        assert_eq!(session.username, "admin");
        assert_eq!(session.role, ADMIN_ROLE);
    }

    #[test]
    fn tampered_session_fails_validation() {
        let mut token = create_session_token("admin", "secret", 1_000, "nonce").expect("token");
        token.push('x');
        assert!(validate_session_token("secret", &token, 999).is_none());
    }

    #[test]
    fn expired_session_fails_validation() {
        let token = create_session_token("admin", "secret", 1_000, "nonce").expect("token");
        assert!(validate_session_token("secret", &token, 1_000).is_none());
    }

    #[test]
    fn admin_password_without_session_secret_is_not_configured() {
        assert!(!admin_config_ready("password", ""));
        assert!(!admin_config_ready("", "secret"));
        assert!(admin_config_ready("password", "secret"));
    }

    #[test]
    fn debug_requests_require_admin() {
        assert!(debug_request_allowed(false, false, true));
        assert!(!debug_request_allowed(false, true, true));
        assert!(!debug_request_allowed(false, false, false));
        assert!(debug_request_allowed(true, true, true));
        assert!(debug_request_allowed(true, false, false));
    }
}

/// OpenAI-compatible chat completions endpoint for `LibreChat`
#[utoipa::path(
    post,
    path = "/v1/chat/completions",
    request_body = ChatCompletionRequest,
    responses(
        (status = 200, description = "Chat completion response", body = ChatCompletionResponse),
        (status = 500, description = "Internal Server Error")
    )
)]
#[allow(clippy::too_many_lines)]
async fn chat_completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Json(req): axum::Json<ChatCompletionRequest>,
) -> Response {
    let is_streaming = req.stream.unwrap_or(false);
    let is_admin = request_is_admin(&state.config, &headers);
    let mut execute = req.execute.unwrap_or(!is_admin);
    let debug_output = req.dry_run.unwrap_or(false);
    if let Some(response) = debug_request_guard(is_admin, debug_output, execute) {
        return response;
    }
    let execute_enabled = state
        .config
        .execute_enabled
        .parse::<bool>()
        .unwrap_or(false);
    if execute && !execute_enabled {
        execute = false;
    }
    info!(
        "Chat completion request for model: {} (streaming: {})",
        req.model, is_streaming
    );
    debug!("Received request with {} messages", req.messages.len());

    // Get the last user message
    let last_user_message = req
        .messages
        .iter()
        .rfind(|m| m.role == "user")
        .map(|m| m.content.clone())
        .unwrap_or_default();

    debug!("Extracted last user message: {}", last_user_message);
    let started_at = Instant::now();

    if is_streaming {
        let id = format!("chatcmpl-{}", Uuid::new_v4());
        let model = req.model.clone();
        let created = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let content = match run_ir_pipeline(
            &state,
            &req.model,
            &last_user_message,
            execute,
            debug_output,
        )
        .await
        {
            Ok(c) => c,
            Err(e) => user_facing_error_message(debug_output, &e),
        };
        let execution_ms = started_at.elapsed().as_millis() as u64;
        record_history_entry(
            &state,
            &last_user_message,
            content.clone(),
            true,
            None,
            execution_ms,
        )
        .await;

        let sse_stream = async_stream::stream! {
            for chunk in content.as_bytes().chunks(800) {
                let delta = Delta {
                    role: None,
                    content: Some(String::from_utf8_lossy(chunk).to_string()),
                };
                let chunk_resp = ChatCompletionChunk {
                    id: id.clone(),
                    object: "chat.completion.chunk".to_string(),
                    created,
                    model: model.clone(),
                    choices: vec![ChoiceChunk {
                        index: 0,
                        delta,
                        finish_reason: None,
                    }],
                };
                yield Ok::<Event, std::convert::Infallible>(Event::default().json_data(chunk_resp).unwrap());
            }

            let final_chunk = ChatCompletionChunk {
                id: id.clone(),
                object: "chat.completion.chunk".to_string(),
                created,
                model: model.clone(),
                choices: vec![ChoiceChunk {
                    index: 0,
                    delta: Delta { role: None, content: None },
                    finish_reason: Some("stop".to_string()),
                }],
            };
            yield Ok(Event::default().json_data(final_chunk).unwrap());
            yield Ok(Event::default().data("[DONE]"));
        };

        Sse::new(sse_stream)
            .keep_alive(KeepAlive::default())
            .into_response()
    } else {
        match run_ir_pipeline(
            &state,
            &req.model,
            &last_user_message,
            execute,
            debug_output,
        )
        .await
        {
            Ok(content) => {
                let execution_ms = started_at.elapsed().as_millis() as u64;
                record_history_entry(
                    &state,
                    &last_user_message,
                    content.clone(),
                    true,
                    None,
                    execution_ms,
                )
                .await;
                let res = ChatCompletionResponse {
                    id: format!("chatcmpl-{}", Uuid::new_v4()),
                    object: "chat.completion".to_string(),
                    created: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs(),
                    model: req.model,
                    choices: vec![Choice {
                        index: 0,
                        message: Message {
                            role: "assistant".to_string(),
                            content,
                            name: None,
                        },
                        finish_reason: "stop".to_string(),
                    }],
                    usage: Usage {
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        total_tokens: 0,
                    },
                };
                (StatusCode::OK, axum::Json(res)).into_response()
            }
            Err(e) => {
                let content = user_facing_error_message(debug_output, &e);
                let execution_ms = started_at.elapsed().as_millis() as u64;
                record_history_entry(
                    &state,
                    &last_user_message,
                    content.clone(),
                    false,
                    Some(e.to_string()),
                    execution_ms,
                )
                .await;
                let res = ChatCompletionResponse {
                    id: format!("chatcmpl-{}", Uuid::new_v4()),
                    object: "chat.completion".to_string(),
                    created: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs(),
                    model: req.model,
                    choices: vec![Choice {
                        index: 0,
                        message: Message {
                            role: "assistant".to_string(),
                            content,
                            name: None,
                        },
                        finish_reason: "stop".to_string(),
                    }],
                    usage: Usage {
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        total_tokens: 0,
                    },
                };
                (StatusCode::OK, axum::Json(res)).into_response()
            }
        }
    }
}

async fn chat_completions_stream(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Json(req): axum::Json<ChatCompletionRequest>,
) -> Response {
    let is_admin = request_is_admin(&state.config, &headers);
    let mut execute = req.execute.unwrap_or(!is_admin);
    let debug_output = req.dry_run.unwrap_or(false);
    if let Some(response) = debug_request_guard(is_admin, debug_output, execute) {
        return response;
    }
    let execute_enabled = state
        .config
        .execute_enabled
        .parse::<bool>()
        .unwrap_or(false);
    if execute && !execute_enabled {
        execute = false;
    }
    let last_user_message = req
        .messages
        .iter()
        .rfind(|m| m.role == "user")
        .map(|m| m.content.clone())
        .unwrap_or_default();
    let model = req.model.clone();

    let sse_stream = async_stream::stream! {
        let started_at = Instant::now();
        let event_id = Uuid::new_v4().to_string();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<StreamUpdate>();
        let task_state = state.clone();
        let task_model = model.clone();
        let task_message = last_user_message.clone();

        yield Ok::<Event, std::convert::Infallible>(
            Event::default()
                .event("stage")
                .json_data(serde_json::json!({
                    "id": event_id,
                    "stage": "received",
                    "status": "completed",
                    "message": "Request received.",
                    "elapsed_ms": 0
                }))
                .unwrap(),
        );

        tokio::spawn(async move {
            let task_started_at = Instant::now();
            let progress_tx = tx.clone();
            let progress = move |event: PipelineProgressEvent| {
                let _ = progress_tx.send(StreamUpdate::Stage(event));
            };
            let result = run_ir_pipeline_with_progress(
                &task_state,
                &task_model,
                &task_message,
                execute,
                debug_output,
                Some(&progress),
            )
            .await;
            let execution_ms = task_started_at.elapsed().as_millis() as u64;
            match result {
                Ok(content) => {
                    record_history_entry(
                        &task_state,
                        &task_message,
                        content.clone(),
                        true,
                        None,
                        execution_ms,
                    )
                    .await;
                    let _ = tx.send(StreamUpdate::Stage(PipelineProgressEvent::stage(
                        "answer",
                        "running",
                        "Answer ready. Streaming response.",
                    )));
                    let _ = tx.send(StreamUpdate::Answer(content));
                    let _ = tx.send(StreamUpdate::Done("Done.".to_string()));
                }
                Err(e) => {
                    let content = user_facing_error_message(debug_output, &e);
                    record_history_entry(
                        &task_state,
                        &task_message,
                        content.clone(),
                        false,
                        Some(e.to_string()),
                        execution_ms,
                    )
                    .await;
                    let _ = tx.send(StreamUpdate::Error(content));
                    let _ = tx.send(StreamUpdate::Done("Done with error.".to_string()));
                }
            }
        });

        while let Some(update) = rx.recv().await {
            match update {
                StreamUpdate::Stage(event) => {
                    yield Ok(
                        Event::default()
                            .event("stage")
                            .json_data(stage_payload(&event_id, started_at, &event))
                            .unwrap(),
                    );
                }
                StreamUpdate::Answer(content) => {
                    for chunk in content.as_bytes().chunks(1200) {
                        yield Ok(
                            Event::default()
                                .event("answer")
                                .json_data(serde_json::json!({
                                    "id": event_id,
                                    "content": String::from_utf8_lossy(chunk).to_string(),
                                    "elapsed_ms": started_at.elapsed().as_millis()
                                }))
                                .unwrap(),
                        );
                    }
                }
                StreamUpdate::Error(content) => {
                    yield Ok(
                        Event::default()
                            .event("error")
                            .json_data(serde_json::json!({
                                "id": event_id,
                                "message": content,
                                "elapsed_ms": started_at.elapsed().as_millis(),
                            }))
                            .unwrap(),
                    );
                }
                StreamUpdate::Done(message) => {
                    yield Ok(
                        Event::default()
                            .event("done")
                            .json_data(serde_json::json!({
                                "id": event_id,
                                "message": message,
                                "elapsed_ms": started_at.elapsed().as_millis()
                            }))
                            .unwrap(),
                    );
                    break;
                }
            }
        }
    };

    Sse::new(sse_stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// Execute a GraphQL query against the configured GraphQL API.
/// This endpoint acts as a tool for Open `WebUI` or direct use.
#[utoipa::path(
    post,
    path = "/graphql/query",
    request_body = GraphQLToolRequest,
    responses(
        (status = 200, description = "GraphQL response", body = GraphQLToolResponse),
        (status = 500, description = "Internal Server Error")
    )
)]
async fn graphql_query(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Json(req): axum::Json<GraphQLToolRequest>,
) -> impl IntoResponse {
    info!("Accessing URL: /graphql/query");
    if let Some(response) = admin_guard(&state.config, &headers) {
        return response;
    }

    if !direct_graphql_query_enabled(&state.config) {
        return graphql_json_error(
            StatusCode::FORBIDDEN,
            "/graphql/query is disabled. Set DIRECT_GRAPHQL_QUERY_ENABLED=true to enable validated direct GraphQL execution.",
        );
    }

    let schema_registry = state.schema_registry.read().await.clone();
    if let Err(e) = validate_direct_graphql_query(&schema_registry, &req.query) {
        return graphql_json_error(
            StatusCode::BAD_REQUEST,
            format!("Direct GraphQL validation failed: {e}"),
        );
    }
    if !direct_graphql_variables_are_empty(&req.variables) {
        return graphql_json_error(
            StatusCode::BAD_REQUEST,
            "Direct GraphQL variables are not accepted; inline literal arguments so the request can be fully validated.",
        );
    }

    let bearer_token = if state.config.graph.bearer_token.is_empty() {
        None
    } else {
        Some(state.config.graph.bearer_token.as_str())
    };
    let api_key_header = if state.config.graph.api_key_header.is_empty() {
        None
    } else {
        Some(state.config.graph.api_key_header.as_str())
    };
    let api_key = if state.config.graph.api_key.is_empty() {
        None
    } else {
        Some(state.config.graph.api_key.as_str())
    };

    match execute_graphql(
        &state.client,
        &state.config.graph.graph_endpoint,
        bearer_token,
        api_key_header,
        api_key,
        &req.query,
        &req.variables,
    )
    .await
    {
        Ok(body) => {
            let out = GraphQLToolResponse {
                data: body.get("data").cloned(),
                errors: body.get("errors").cloned(),
            };
            (StatusCode::OK, axum::Json(out)).into_response()
        }
        Err(e) => {
            error!("GraphQL error processing /graphql/query: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response()
        }
    }
}
