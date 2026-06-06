use crate::agent::execute_graphql;
use crate::error::{PipelineError, PipelineResult};
use crate::history::HistoryEntry;
use crate::openai::GraphQLToolResponse;
use crate::pipeline::run_ir_pipeline;
use crate::planner::{
    PlanV2, apply_parent_relation_rewrite, plan_v2_to_multistep, resolve_sls_metric_refs,
    validate_plan_v2, validate_sls_metric_sources,
};
use crate::provider::{infer_provider_kind, prompt_cache_profile};
use crate::query_executor::execute_multistep_plan_with_progress;
use crate::{
    AppState, direct_graphql_query_enabled, direct_graphql_variables_are_empty,
    persist_history_to_disk, validate_direct_graphql_query,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Instant;

#[derive(Clone)]
pub(crate) struct ZephyrService {
    state: Arc<AppState>,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct AskZephyrRequest {
    pub(crate) question: String,
    #[serde(default = "default_execute")]
    pub(crate) execute: bool,
    #[serde(default)]
    pub(crate) include_debug: bool,
    #[serde(default)]
    pub(crate) model: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct AskZephyrResponse {
    pub(crate) answer: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) evidence: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) grounding_confidence: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) uncertainty: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) plan_steps: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) executed_queries: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) metrics: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) raw_debug_output: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct InspectSchemaRequest {
    #[serde(default)]
    pub(crate) question: Option<String>,
    #[serde(default)]
    pub(crate) limit: Option<usize>,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct HistoryRequest {
    #[serde(default)]
    pub(crate) limit: Option<usize>,
    #[serde(default)]
    pub(crate) search: Option<String>,
    #[serde(default)]
    pub(crate) include_debug: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct DirectGraphqlRequest {
    pub(crate) query: String,
    #[serde(default)]
    pub(crate) variables: Value,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct ExecutePlanRequest {
    pub(crate) plan_v2: Value,
    #[serde(default)]
    pub(crate) model: Option<String>,
    #[serde(default)]
    pub(crate) question: Option<String>,
}

fn default_execute() -> bool {
    true
}

impl ZephyrService {
    pub(crate) fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    pub(crate) async fn ask(&self, req: AskZephyrRequest) -> PipelineResult<AskZephyrResponse> {
        let question = req.question.trim();
        if question.is_empty() {
            return Err(PipelineError::planning("question cannot be empty"));
        }
        let model = req.model.as_deref().unwrap_or("");
        let started = Instant::now();
        let debug_output = run_ir_pipeline(&self.state, model, question, req.execute, true).await?;
        let provenance = fenced_json_after_label(&debug_output, "Provenance:");
        let answer = provenance
            .as_ref()
            .and_then(|p| p.get("answer"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .or_else(|| final_answer_from_debug_output(&debug_output))
            .unwrap_or_else(|| debug_output.clone());
        let response = AskZephyrResponse {
            answer: answer.clone(),
            evidence: provenance.as_ref().and_then(|p| p.get("evidence").cloned()),
            grounding_confidence: provenance
                .as_ref()
                .and_then(|p| p.get("grounding_confidence").cloned())
                .or_else(|| fenced_json_after_label(&debug_output, "grounding_confidence:")),
            uncertainty: provenance
                .as_ref()
                .and_then(|p| p.get("uncertainty").cloned()),
            plan_steps: req.include_debug.then(|| {
                provenance
                    .as_ref()
                    .and_then(|p| p.get("plan_steps").cloned())
                    .unwrap_or(Value::Null)
            }),
            executed_queries: req.include_debug.then(|| {
                provenance
                    .as_ref()
                    .and_then(|p| p.get("executed_queries").cloned())
                    .unwrap_or(Value::Null)
            }),
            metrics: req.include_debug.then(|| {
                provenance
                    .as_ref()
                    .and_then(|p| p.get("metrics").cloned())
                    .unwrap_or(Value::Null)
            }),
            raw_debug_output: req.include_debug.then_some(debug_output),
        };
        self.record_history(
            question,
            answer,
            true,
            None,
            started.elapsed().as_millis() as u64,
        )
        .await;
        Ok(response)
    }

    pub(crate) async fn plan_query(&self, question: &str) -> PipelineResult<Value> {
        let question = question.trim();
        if question.is_empty() {
            return Err(PipelineError::planning("question cannot be empty"));
        }
        let output = run_ir_pipeline(&self.state, "", question, false, true).await?;
        Ok(json!({
            "planner_raw_plan_json": fenced_json_after_label(&output, "Raw Planner JSON:"),
            "planner_repair_raw_plan_json": fenced_json_after_label(&output, "Raw Repair JSON:"),
            "debug_output": output
        }))
    }

    pub(crate) async fn inspect_schema(&self, req: InspectSchemaRequest) -> Value {
        let question = req.question.unwrap_or_default();
        let registry = self.state.schema_registry.read().await.clone();
        let budget = registry.planner_retrieval_budget(&question);
        let root_limit = req.limit.unwrap_or(budget.root_limit).clamp(1, 12);
        let slice = registry.schema_retrieval_slice(&question, root_limit, budget.field_limit);
        json!({
            "mode": "schema_retrieval",
            "budget": {
                "root_limit": root_limit,
                "field_limit": budget.field_limit,
                "entity_resolution_limit": budget.entity_resolution_limit
            },
            "intent": slice.intent,
            "profile": slice.profile,
            "roots": slice.roots
        })
    }

    pub(crate) async fn history(&self, req: HistoryRequest) -> Value {
        let limit = req.limit.unwrap_or(25).clamp(1, 100);
        let entries = {
            let history = self.state.history.read().await;
            if let Some(search) = req.search.as_deref().filter(|s| !s.trim().is_empty()) {
                history.search(search)
            } else {
                history.get_recent(limit)
            }
        };
        let items = entries
            .into_iter()
            .take(limit)
            .map(|entry| safe_history_entry(entry, req.include_debug))
            .collect::<Vec<_>>();
        json!({ "items": items })
    }

    pub(crate) async fn direct_graphql(
        &self,
        req: DirectGraphqlRequest,
        mcp_debug_authorized: bool,
    ) -> Result<GraphQLToolResponse, String> {
        if !direct_graphql_query_enabled(&self.state.config) && !mcp_debug_authorized {
            return Err("direct GraphQL execution is disabled".to_string());
        }
        let registry = self.state.schema_registry.read().await.clone();
        validate_direct_graphql_query(&registry, &req.query)
            .map_err(|e| format!("Direct GraphQL validation failed: {e}"))?;
        if !direct_graphql_variables_are_empty(&req.variables) {
            return Err(
                "Direct GraphQL variables are not accepted; inline literal arguments so the request can be fully validated."
                    .to_string(),
            );
        }
        let bearer_token = non_empty(&self.state.config.graph.bearer_token);
        let api_key_header = non_empty(&self.state.config.graph.api_key_header);
        let api_key = non_empty(&self.state.config.graph.api_key);
        let response = execute_graphql(
            &self.state.client,
            &self.state.config.graph.graph_endpoint,
            bearer_token,
            api_key_header,
            api_key,
            &req.query,
            &Value::Null,
        )
        .await
        .map_err(|e| e.to_string())?;
        Ok(GraphQLToolResponse {
            data: response.get("data").cloned(),
            errors: response.get("errors").cloned(),
        })
    }

    pub(crate) async fn execute_plan(&self, req: ExecutePlanRequest) -> PipelineResult<Value> {
        let mut plan: PlanV2 = serde_json::from_value(req.plan_v2)
            .map_err(|e| PipelineError::planning(format!("invalid PlanV2 JSON: {e}")))?;
        let registry = self.state.schema_registry.read().await.clone();
        resolve_sls_metric_refs(&mut plan, self.state.sls.as_ref())
            .map_err(PipelineError::planning)?;
        validate_sls_metric_sources(&plan, &registry, self.state.sls.as_ref())
            .map_err(PipelineError::planning)?;
        apply_parent_relation_rewrite(
            &mut plan,
            req.question.as_deref().unwrap_or(""),
            &registry,
            self.state.sls.as_ref(),
        );
        validate_plan_v2(&plan, &registry)?;
        let multi_step = plan_v2_to_multistep(&plan)
            .ok_or_else(|| PipelineError::planning("PlanV2 did not compile to executable steps"))?;
        let (deterministic, effective_queries, evidence, groundings) =
            execute_multistep_plan_with_progress(
                &self.state,
                &registry,
                req.model.as_deref().unwrap_or(""),
                req.question.as_deref().unwrap_or("MCP execute_plan"),
                &multi_step,
                None,
            )
            .await?;
        Ok(json!({
            "deterministic_answer": deterministic,
            "effective_queries": effective_queries,
            "evidence": {
                "row_count": evidence.row_count,
                "sample_rows": evidence.sample_rows,
                "field_values": evidence.field_values,
                "literal_sample": evidence.literals.into_iter().take(20).collect::<Vec<_>>(),
                "time_values_sample": evidence.time_values.into_iter().take(10).collect::<Vec<_>>()
            },
            "execution_groundings": groundings
        }))
    }

    pub(crate) async fn runtime_summary(&self) -> Value {
        let schema_meta = self.state.schema_meta.read().await;
        let provider = infer_provider_kind(&self.state.config, &self.state.config.model);
        json!({
            "llm_provider": self.state.config.llm_provider,
            "model": self.state.config.model,
            "provider_prompt_cache": prompt_cache_profile(provider),
            "execute_enabled": self.state.config.execute_enabled,
            "mcp_enabled": self.state.config.zephyr_mcp_enabled,
            "http_enabled": self.state.config.zephyr_http_enabled,
            "mcp_transport": self.state.config.mcp_transport,
            "mcp_debug_tools_enabled": self.state.config.mcp_debug_tools_enabled,
            "direct_graphql_query_enabled": self.state.config.graph.direct_query_enabled,
            "schema_source": schema_meta.source.as_str(),
            "schema_loaded_at": schema_meta.loaded_at.to_rfc3339(),
            "sls_loaded": self.state.sls.is_some()
        })
    }

    pub(crate) async fn sls_summary(&self) -> Value {
        let Some(sls) = self.state.sls.as_ref() else {
            return json!({ "loaded": false });
        };
        let mut concepts = sls.concepts.keys().cloned().collect::<Vec<_>>();
        concepts.sort();
        let mut metrics = sls
            .metrics
            .as_ref()
            .map(|m| m.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        metrics.sort();
        json!({
            "loaded": true,
            "concept_count": concepts.len(),
            "metric_count": metrics.len(),
            "concepts": concepts.into_iter().take(50).collect::<Vec<_>>(),
            "metrics": metrics.into_iter().take(50).collect::<Vec<_>>(),
            "preferred_join_path_count": sls.preferred_join_paths.len(),
            "root_field_role_count": sls.field_roles_by_root.len(),
            "type_field_role_count": sls.field_roles_by_type.len()
        })
    }

    async fn record_history(
        &self,
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
        let mut history = self.state.history.write().await;
        history.add(entry);
        persist_history_to_disk(&self.state.history_path, &history);
    }
}

fn non_empty(value: &str) -> Option<&str> {
    (!value.is_empty()).then_some(value)
}

fn safe_history_entry(entry: HistoryEntry, include_debug: bool) -> Value {
    json!({
        "id": entry.id,
        "question": entry.question,
        "answer": entry.answer.as_deref().map(|answer| {
            if include_debug {
                answer.to_string()
            } else {
                answer_preview(answer, 500)
            }
        }),
        "execution_ms": entry.execution_ms,
        "success": entry.success,
        "error": entry.error,
        "timestamp": entry.timestamp,
        "plan": include_debug.then_some(entry.plan).flatten()
    })
}

fn answer_preview(answer: &str, max_chars: usize) -> String {
    let mut out = answer.chars().take(max_chars).collect::<String>();
    if answer.chars().count() > max_chars {
        out.push_str("...");
    }
    out
}

fn final_answer_from_debug_output(output: &str) -> Option<String> {
    let marker = "Final Answer:\n";
    let start = output.find(marker)? + marker.len();
    let tail = &output[start..];
    let end = tail.find("\n\nProvenance:").unwrap_or(tail.len());
    let answer = tail[..end].trim();
    (!answer.is_empty()).then_some(answer.to_string())
}

fn fenced_json_after_label(output: &str, label: &str) -> Option<Value> {
    let label_start = output.find(label)?;
    let tail = &output[label_start + label.len()..];
    let fence_start = tail.find("```json")?;
    let json_start = fence_start + "```json".len();
    let json_tail = tail[json_start..].trim_start_matches(['\r', '\n']);
    let fence_end = json_tail.find("```")?;
    serde_json::from_str(json_tail[..fence_end].trim()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn final_answer_parser_extracts_answer_without_provenance() {
        let output = "Plan...\n\nFinal Answer:\nDone.\n\nProvenance:\n```json\n{}\n```";
        assert_eq!(
            final_answer_from_debug_output(output),
            Some("Done.".to_string())
        );
    }

    #[test]
    fn fenced_json_parser_extracts_named_block() {
        let output = "Provenance:\n```json\n{\"answer\":\"ok\"}\n```";
        let parsed = fenced_json_after_label(output, "Provenance:").expect("json block");
        assert_eq!(parsed["answer"], "ok");
    }
}
