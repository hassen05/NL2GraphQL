use crate::AppState;
use crate::agent::create_answer_agent;
use crate::prompts::{AnswerSynthesisPromptContext, build_answer_synthesis_prompt};
use crate::provider::ProviderTokenUsage;
use regex::Regex;
use std::sync::LazyLock;

static UNIT_HALLUCINATION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(mw|kw|gw|kwh|mwh|gwh|m/s)\b").expect("valid unit hallucination regex")
});

pub(crate) struct SynthesizedAnswer {
    pub(crate) text: String,
    pub(crate) token_usage: Option<ProviderTokenUsage>,
}

impl SynthesizedAnswer {
    fn fallback(fallback_answer: &str) -> Self {
        Self {
            text: fallback_answer.to_string(),
            token_usage: None,
        }
    }
}

pub(crate) async fn synthesize_answer_with_llm(
    state: &AppState,
    model_name: &str,
    user_message: &str,
    evidence: &serde_json::Value,
    fallback_answer: &str,
) -> SynthesizedAnswer {
    fn looks_like_refusal(text: &str) -> bool {
        let t = text.to_lowercase();
        (t.contains("i'm unable")
            || t.contains("i am unable")
            || t.contains("cannot provide")
            || t.contains("can't provide")
            || t.contains("necessary data has not been retrieved")
            || t.contains("hasn't been retrieved"))
            && !t.contains("no matching records found")
    }
    fn looks_like_unit_hallucination(text: &str) -> bool {
        UNIT_HALLUCINATION_RE.is_match(text)
    }

    let answer_agent = if model_name.is_empty() || model_name == state.config.model {
        state.cached_answer_agent.clone()
    } else {
        match create_answer_agent(&state.config, model_name).await {
            Ok(agent) => agent,
            Err(_) => return SynthesizedAnswer::fallback(fallback_answer),
        }
    };

    let evidence_text = serde_json::to_string_pretty(evidence).unwrap_or_else(|_| "{}".to_string());
    let evidence_lc = evidence_text.to_lowercase();
    let evidence_has_unit = evidence_lc.contains("\"unit\"") || evidence_lc.contains("units");

    let prompt = build_answer_synthesis_prompt(&AnswerSynthesisPromptContext {
        user_message,
        evidence_text: &evidence_text,
        fallback_answer,
    });

    match answer_agent.prompt_extended(&prompt).await {
        Ok(response) if !response.output.trim().is_empty() => {
            let out = response.output.trim();
            let token_usage = Some(ProviderTokenUsage::from_rig(response.total_usage));
            if looks_like_refusal(out) || (!evidence_has_unit && looks_like_unit_hallucination(out))
            {
                SynthesizedAnswer {
                    text: fallback_answer.to_string(),
                    token_usage,
                }
            } else {
                SynthesizedAnswer {
                    text: out.to_string(),
                    token_usage,
                }
            }
        }
        _ => SynthesizedAnswer::fallback(fallback_answer),
    }
}
