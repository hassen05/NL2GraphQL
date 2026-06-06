use anyhow::Result;
use reqwest::Client;
use rig::agent::PromptResponse;
use rig::client::{CompletionClient, Nothing};
use rig::completion::Prompt;
use rig::providers::{anthropic, ollama, openai};
use serde_json::{Value, json};
use tracing::{debug, info};

use crate::Config;
use crate::provider::{ProviderKind, provider_kind_from_config};

const PLANNER_PREAMBLE: &str =
    "You produce structured JSON only. Do not call tools. Do not output prose.";
const ANSWER_PREAMBLE: &str = "You are a data answer synthesizer. Produce concise natural-language answers only, based strictly on provided evidence. Do not call tools. Do not output JSON unless explicitly requested.";

#[derive(Clone)]
pub(crate) enum LlmAgent {
    Ollama(rig::agent::Agent<ollama::CompletionModel>),
    OpenAi(rig::agent::Agent<openai::completion::CompletionModel>),
    Anthropic(rig::agent::Agent<anthropic::completion::CompletionModel>),
}

impl LlmAgent {
    pub(crate) async fn prompt_extended(&self, prompt: &str) -> Result<PromptResponse> {
        match self {
            Self::Ollama(agent) => Ok(Prompt::prompt(agent, prompt)
                .extended_details()
                .multi_turn(42)
                .await?),
            Self::OpenAi(agent) => Ok(Prompt::prompt(agent, prompt)
                .extended_details()
                .multi_turn(42)
                .await?),
            Self::Anthropic(agent) => Ok(Prompt::prompt(agent, prompt)
                .extended_details()
                .multi_turn(42)
                .await?),
        }
    }

    pub(crate) async fn prompt_text(&self, prompt: &str) -> Result<String> {
        Ok(self.prompt_extended(prompt).await?.output)
    }
}

/// Execute a GraphQL query with optional authentication.
///
/// Supports:
/// - Bearer token authentication (if `bearer_token` is provided)
/// - API key authentication (if both `api_key_header` and `api_key` are provided)
/// - No authentication (if neither is provided)
pub async fn execute_graphql(
    client: &Client,
    url: &str,
    bearer_token: Option<&str>,
    api_key_header: Option<&str>,
    api_key: Option<&str>,
    query: &str,
    variables: &Value,
) -> Result<Value> {
    debug!("Accessing GraphQL URL: {}", url);

    let payload = json!({ "query": query, "variables": variables });

    let mut request = client.post(url).json(&payload);

    // Add authentication if configured
    if let Some(token) = bearer_token
        && !token.is_empty()
    {
        debug!("Using bearer token authentication");
        request = request.header("Authorization", format!("Bearer {token}"));
    }

    if let (Some(header), Some(key)) = (api_key_header, api_key)
        && !header.is_empty()
        && !key.is_empty()
    {
        debug!("Using API key authentication with header: {}", header);
        request = request.header(header, key);
    }

    let response = request.send().await?;
    let status = response.status();
    let body_text = response.text().await?;

    if !status.is_success() {
        let details = if body_text.trim().is_empty() {
            String::new()
        } else {
            format!(" | body: {}", body_text)
        };
        return Err(anyhow::anyhow!(
            "HTTP status {} for url ({}){}",
            status,
            url,
            details
        ));
    }

    let res: Value = serde_json::from_str(&body_text).map_err(|e| {
        anyhow::anyhow!("Failed to parse GraphQL JSON response: {e}; body: {body_text}")
    })?;

    Ok(res)
}

pub async fn create_ir_agent(config: &Config, model_name: &str) -> Result<LlmAgent> {
    create_agent(
        config,
        model_name,
        "Zephyr IR Agent",
        PLANNER_PREAMBLE,
        1024,
    )
    .await
}

pub async fn create_answer_agent(config: &Config, model_name: &str) -> Result<LlmAgent> {
    create_agent(
        config,
        model_name,
        "Zephyr Answer Agent",
        ANSWER_PREAMBLE,
        512,
    )
    .await
}

async fn create_agent(
    config: &Config,
    model_name: &str,
    agent_name: &str,
    preamble: &str,
    max_tokens: u64,
) -> Result<LlmAgent> {
    let provider = provider_kind_from_config(config);
    validate_provider_config_values(provider, &config.openai_api_key, &config.anthropic_api_key)
        .map_err(|e| anyhow::anyhow!(e))?;

    match provider {
        ProviderKind::Ollama => {
            create_ollama_agent(config, model_name, agent_name, preamble, max_tokens)
        }
        ProviderKind::OpenAi => {
            create_openai_agent(config, model_name, agent_name, preamble, max_tokens)
        }
        ProviderKind::Anthropic => {
            create_anthropic_agent(config, model_name, agent_name, preamble, max_tokens)
        }
        ProviderKind::Copilot | ProviderKind::Unknown => Err(anyhow::anyhow!(
            "unsupported LLM_PROVIDER `{}`; expected ollama, openai, or anthropic",
            config.llm_provider
        )),
    }
}

pub(crate) fn validate_provider_config_values(
    provider: ProviderKind,
    openai_api_key: &str,
    anthropic_api_key: &str,
) -> std::result::Result<(), String> {
    match provider {
        ProviderKind::OpenAi if openai_api_key.trim().is_empty() => {
            Err("OPENAI_API_KEY is required when LLM_PROVIDER=openai".to_string())
        }
        ProviderKind::Anthropic if anthropic_api_key.trim().is_empty() => {
            Err("ANTHROPIC_API_KEY is required when LLM_PROVIDER=anthropic".to_string())
        }
        ProviderKind::Unknown => {
            Err("unsupported LLM_PROVIDER; expected ollama, openai, or anthropic".to_string())
        }
        _ => Ok(()),
    }
}

fn create_ollama_agent(
    config: &Config,
    model_name: &str,
    agent_name: &str,
    preamble: &str,
    max_tokens: u64,
) -> Result<LlmAgent> {
    let ollama_client = ollama::Client::builder()
        .base_url(&config.ollama_url)
        .api_key(Nothing)
        .build()
        .map_err(|e| anyhow::anyhow!("failed to create ollama client: {e}"))?;

    let target_model = if model_name.is_empty() {
        &config.model
    } else {
        model_name
    };

    info!("Using ollama model: {}", target_model);
    let model = ollama_client.completion_model(target_model);

    let agent = rig::agent::AgentBuilder::new(model)
        .name(agent_name)
        .preamble(preamble)
        .max_tokens(max_tokens)
        .build();

    Ok(LlmAgent::Ollama(agent))
}

fn create_openai_agent(
    config: &Config,
    model_name: &str,
    agent_name: &str,
    preamble: &str,
    max_tokens: u64,
) -> Result<LlmAgent> {
    let mut builder = openai::Client::builder().api_key(config.openai_api_key.clone());
    if !config.openai_base_url.trim().is_empty() {
        builder = builder.base_url(config.openai_base_url.trim());
    }
    let openai_client = builder
        .build()
        .map_err(|e| anyhow::anyhow!("failed to create openai client: {e}"))?
        .completions_api();
    let target_model = if model_name.is_empty() {
        config.openai_model.as_str()
    } else {
        model_name
    };

    info!("Using openai model: {}", target_model);
    let model = openai_client.completion_model(target_model);
    let agent = rig::agent::AgentBuilder::new(model)
        .name(agent_name)
        .preamble(preamble)
        .max_tokens(max_tokens)
        .build();

    Ok(LlmAgent::OpenAi(agent))
}

fn create_anthropic_agent(
    config: &Config,
    model_name: &str,
    agent_name: &str,
    preamble: &str,
    max_tokens: u64,
) -> Result<LlmAgent> {
    let anthropic_client = anthropic::Client::new(config.anthropic_api_key.clone())
        .map_err(|e| anyhow::anyhow!("failed to create anthropic client: {e}"))?;
    let target_model = if model_name.is_empty() {
        config.anthropic_model.as_str()
    } else {
        model_name
    };

    info!("Using anthropic model: {}", target_model);
    let model = anthropic_client.completion_model(target_model);
    let agent = rig::agent::AgentBuilder::new(model)
        .name(agent_name)
        .preamble(preamble)
        .max_tokens(max_tokens)
        .build();

    Ok(LlmAgent::Anthropic(agent))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_config_rejects_missing_openai_key() {
        let err = validate_provider_config_values(ProviderKind::OpenAi, "", "")
            .expect_err("missing OpenAI key should fail");
        assert!(err.contains("OPENAI_API_KEY"));
    }

    #[test]
    fn provider_config_rejects_missing_anthropic_key() {
        let err = validate_provider_config_values(ProviderKind::Anthropic, "", "")
            .expect_err("missing Anthropic key should fail");
        assert!(err.contains("ANTHROPIC_API_KEY"));
    }

    #[test]
    fn provider_config_accepts_default_ollama_without_api_keys() {
        validate_provider_config_values(ProviderKind::Ollama, "", "")
            .expect("ollama should not require provider API keys");
    }
}
