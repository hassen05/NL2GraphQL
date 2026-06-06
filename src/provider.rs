use serde::Serialize;
use std::ops::AddAssign;

use crate::Config;

/// Provider-level prompt-cache behavior.
///
/// This is intentionally separated from the app-side planner cache:
/// - app-side cache reuses exact planner/repair responses inside this process
/// - provider prompt caching depends on the model vendor and billing API
/// - backend execution and final answers are still never cached here
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProviderKind {
    Anthropic,
    OpenAi,
    Copilot,
    Ollama,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PromptCacheMode {
    ExplicitCacheControl,
    AutomaticPrefix,
    None,
    Unknown,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ProviderPromptCacheProfile {
    pub(crate) provider: ProviderKind,
    pub(crate) cache_mode: PromptCacheMode,
    pub(crate) provider_cache_available: bool,
    pub(crate) stable_prefix_required: bool,
    pub(crate) ttl: Option<&'static str>,
    pub(crate) api_knob: Option<&'static str>,
    pub(crate) target_prompt_shape: &'static str,
    pub(crate) notes: Vec<&'static str>,
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub(crate) struct ProviderTokenUsage {
    pub(crate) prompt_tokens: u64,
    pub(crate) completion_tokens: u64,
    pub(crate) total_tokens: u64,
}

impl ProviderTokenUsage {
    pub(crate) fn from_rig(usage: rig::completion::Usage) -> Self {
        Self {
            prompt_tokens: usage.input_tokens,
            completion_tokens: usage.output_tokens,
            total_tokens: usage.total_tokens,
        }
    }

    pub(crate) fn is_available(self) -> bool {
        self.total_tokens > 0 || self.prompt_tokens > 0 || self.completion_tokens > 0
    }
}

impl AddAssign for ProviderTokenUsage {
    fn add_assign(&mut self, rhs: Self) {
        self.prompt_tokens += rhs.prompt_tokens;
        self.completion_tokens += rhs.completion_tokens;
        self.total_tokens += rhs.total_tokens;
    }
}

pub(crate) fn provider_kind_from_name(name: &str) -> ProviderKind {
    match name.trim().to_ascii_lowercase().as_str() {
        "anthropic" => ProviderKind::Anthropic,
        "openai" => ProviderKind::OpenAi,
        "ollama" => ProviderKind::Ollama,
        "copilot" => ProviderKind::Copilot,
        _ => ProviderKind::Unknown,
    }
}

pub(crate) fn provider_kind_from_config(config: &Config) -> ProviderKind {
    provider_kind_from_name(&config.llm_provider)
}

pub(crate) fn infer_provider_kind(config: &Config, model_name: &str) -> ProviderKind {
    let configured = provider_kind_from_config(config);
    if configured != ProviderKind::Unknown {
        return configured;
    }

    let effective_model = if model_name.is_empty() {
        config.model.as_str()
    } else {
        model_name
    };
    let probe = format!(
        "{} {} {}",
        effective_model, config.ollama_url, config.graph.graph_endpoint
    )
    .to_ascii_lowercase();

    if probe.contains("anthropic") || probe.contains("claude") {
        ProviderKind::Anthropic
    } else if probe.contains("copilot") || probe.contains("github") {
        ProviderKind::Copilot
    } else if probe.contains("openai") || probe.starts_with("gpt-") || probe.contains(" gpt-") {
        ProviderKind::OpenAi
    } else if probe.contains("ollama") || probe.contains("localhost") || probe.contains("127.0.0.1")
    {
        ProviderKind::Ollama
    } else {
        ProviderKind::Unknown
    }
}

pub(crate) fn prompt_cache_profile(provider: ProviderKind) -> ProviderPromptCacheProfile {
    match provider {
        ProviderKind::Anthropic => ProviderPromptCacheProfile {
            provider,
            cache_mode: PromptCacheMode::ExplicitCacheControl,
            provider_cache_available: true,
            stable_prefix_required: true,
            ttl: Some("5 minutes by default; 1 hour is opt-in"),
            api_knob: Some("cache_control breakpoints"),
            target_prompt_shape: "stable prefix: schema + few-shots + SLS hints; variable suffix: history + current question",
            notes: vec![
                "Anthropic prompt caching is explicit; callers must mark cacheable prefix breakpoints.",
                "Provider benchmarks must report cached-vs-uncached p50 latency, not only request success.",
            ],
        },
        ProviderKind::OpenAi => ProviderPromptCacheProfile {
            provider,
            cache_mode: PromptCacheMode::AutomaticPrefix,
            provider_cache_available: true,
            stable_prefix_required: true,
            ttl: None,
            api_knob: None,
            target_prompt_shape: "stable prefix: schema + few-shots + SLS hints; variable suffix: history + current question",
            notes: vec![
                "OpenAI prefix caching is automatic; there is no app-side cache_control knob.",
                "Keep large, stable prompt material byte-identical across requests to maximize provider cache hits.",
            ],
        },
        ProviderKind::Copilot => ProviderPromptCacheProfile {
            provider,
            cache_mode: PromptCacheMode::AutomaticPrefix,
            provider_cache_available: true,
            stable_prefix_required: true,
            ttl: None,
            api_knob: None,
            target_prompt_shape: "stable prefix: schema + few-shots + SLS hints; variable suffix: history + current question",
            notes: vec![
                "Copilot-hosted model prefix caching is treated like automatic provider prefix caching.",
                "Confirm actual latency/cost behavior in the Week 1 provider spike.",
            ],
        },
        ProviderKind::Ollama => ProviderPromptCacheProfile {
            provider,
            cache_mode: PromptCacheMode::None,
            provider_cache_available: false,
            stable_prefix_required: true,
            ttl: None,
            api_knob: None,
            target_prompt_shape: "stable prefix: schema + few-shots + SLS hints; variable suffix: history + current question",
            notes: vec![
                "Ollama does not provide cross-request provider prefix caching.",
                "Use app-side prompt preparation and exact-response caches for local development only.",
            ],
        },
        ProviderKind::Unknown => ProviderPromptCacheProfile {
            provider,
            cache_mode: PromptCacheMode::Unknown,
            provider_cache_available: false,
            stable_prefix_required: true,
            ttl: None,
            api_knob: None,
            target_prompt_shape: "stable prefix: schema + few-shots + SLS hints; variable suffix: history + current question",
            notes: vec![
                "Unknown provider; do not assume provider-side prompt caching until measured.",
            ],
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_kind_from_name_accepts_supported_values() {
        assert_eq!(provider_kind_from_name("ollama"), ProviderKind::Ollama);
        assert_eq!(provider_kind_from_name("OPENAI"), ProviderKind::OpenAi);
        assert_eq!(
            provider_kind_from_name(" anthropic "),
            ProviderKind::Anthropic
        );
    }

    #[test]
    fn provider_kind_from_name_rejects_unknown_values() {
        assert_eq!(provider_kind_from_name("bedrock"), ProviderKind::Unknown);
        assert_eq!(provider_kind_from_name(""), ProviderKind::Unknown);
    }

    #[test]
    fn cache_profiles_match_provider_contracts() {
        assert_eq!(
            prompt_cache_profile(ProviderKind::Ollama).cache_mode,
            PromptCacheMode::None
        );
        assert_eq!(
            prompt_cache_profile(ProviderKind::OpenAi).cache_mode,
            PromptCacheMode::AutomaticPrefix
        );
        assert_eq!(
            prompt_cache_profile(ProviderKind::Anthropic).cache_mode,
            PromptCacheMode::ExplicitCacheControl
        );
    }
}
