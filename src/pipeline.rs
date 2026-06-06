use crate::AppState;
use crate::agent::create_ir_agent;
use crate::answer_synthesis::synthesize_answer_with_llm;
use crate::capabilities::CapabilityGraph;
use crate::entity_linker::{
    GroundedEntityMatch, ResolutionStatus, extracted_entity_mentions,
    render_entity_mention_hints_block, render_entity_resolution_block_from_resolutions,
    resolve_entity_resolutions, resolve_grounded_entity_resolutions,
};
use crate::error::{PipelineError, PipelineResult};
use crate::introspection::introspection_answer;
use crate::planner::{
    ExecutedArtifactKind, apply_parent_relation_rewrite, executed_query_text,
    extract_identifier_candidates, extract_plan_v2_json_from_response,
    parse_plan_v2_struct_from_response, plan_v2_to_multistep, render_effective_queries,
    render_multistep_plan, resolve_sls_metric_refs, scope_guard_message, scope_used_summary,
    synthesize_simple_fetch_plan, validate_plan_v2, validate_sls_metric_sources,
};
use crate::planner_cache::{
    PlannerContextCacheEntry, PlannerContextCacheKey, PlannerResponseCacheKey, StaticPromptHints,
};
use crate::policy_guard::{apply_policy_fixes, evaluate_plan_policies, policy_hints_for_prompt};
use crate::progress::{PipelineProgressEvent, ProgressCallback, emit_progress};
use crate::prompt_examples::planner_examples_for_message;
use crate::prompts::{
    PlanRepairPromptContext, PlannerPromptContext, build_plan_repair_prompt, build_planner_prompt,
};
use crate::provider::{
    ProviderPromptCacheProfile, ProviderTokenUsage, infer_provider_kind, prompt_cache_profile,
};
use crate::query_executor::{
    DeterministicAnswer, DeterministicAnswerKind, ExecutionEvidence, ExecutionGrounding,
    execute_multistep_plan_with_progress,
};
use crate::schema_registry::{SchemaRegistry, SchemaRetrievalSlice};
use chrono::Utc;
use graphql_parser::query::{
    Definition as QueryDefinition, OperationDefinition, Selection, Value as QueryValue, parse_query,
};
use std::collections::{BTreeSet, HashMap};
use std::time::Instant;

fn is_entity_like_candidate(candidate: &str) -> bool {
    let tokens = candidate
        .split_whitespace()
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    if tokens.len() >= 2
        && tokens.iter().skip(1).any(|token| {
            token
                .chars()
                .next()
                .is_some_and(|ch| ch.is_ascii_uppercase())
        })
    {
        return true;
    }
    if tokens.len() == 1 {
        let token = tokens[0];
        let has_upper = token.chars().any(|ch| ch.is_ascii_uppercase());
        let has_digit = token.chars().any(|ch| ch.is_ascii_digit());
        let has_id_punct = token.contains('-') || token.contains('_') || token.contains(':');
        return has_upper && (has_digit || has_id_punct);
    }
    false
}

fn user_has_entity_scope_request(schema_registry: &SchemaRegistry, user_message: &str) -> bool {
    let _ = schema_registry;
    extract_identifier_candidates(user_message)
        .into_iter()
        .any(|candidate| is_entity_like_candidate(&candidate))
}

fn is_identifier_like_field_name(field: &str) -> bool {
    let lower = field.to_ascii_lowercase();
    lower == "id"
        || lower.ends_with("id")
        || lower.ends_with("uid")
        || lower.contains("code")
        || lower.contains("ref")
        || lower.contains("key")
}

fn is_label_like_value(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return false;
    }
    let has_space = trimmed.contains(char::is_whitespace);
    let has_alpha = trimmed.chars().any(|ch| ch.is_ascii_alphabetic());
    let has_lower = trimmed.chars().any(|ch| ch.is_ascii_lowercase());
    let id_like = !has_space
        && !has_lower
        && trimmed.chars().all(|ch| {
            ch.is_ascii_uppercase() || ch.is_ascii_digit() || matches!(ch, '-' | '_' | ':')
        });
    has_alpha && !id_like
}

fn matched_scope_constraints_from_json(value: &serde_json::Value) -> Vec<ScopeConstraint> {
    value
        .get("matched_constraints")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|item| scope_constraint_from_json(&item))
        .collect()
}

fn location_label_capability_gap(
    user_message: &str,
    scope_used: &serde_json::Value,
    evidence: &ExecutionEvidence,
    sls: Option<&crate::sls::Sls>,
) -> Option<String> {
    if evidence.row_count != 0 {
        return None;
    }
    let mentions_location =
        sls.is_some_and(|s| s.message_mentions_concept(user_message, "location"));
    if !mentions_location {
        return None;
    }
    let matched = matched_scope_constraints_from_json(scope_used);
    if matched.is_empty() {
        return None;
    }
    let suspicious = matched.iter().all(|constraint| {
        is_identifier_like_field_name(&constraint.field)
            && constraint
                .values
                .iter()
                .all(|value| is_label_like_value(value))
    });
    if !suspicious {
        return None;
    }
    Some(
        "I can’t answer with the requested scope because the available location filters are identifier-based and did not resolve the requested location label."
            .to_string(),
    )
}

fn plan_has_explicit_scope_step(plan: &crate::planner::MultiStepPlan) -> bool {
    plan.steps.iter().any(|step| match &step.op {
        crate::planner::PlanV2Op::Fetch { filter, .. } => filter.is_some(),
        crate::planner::PlanV2Op::FilterRows { value, .. } => match value {
            serde_json::Value::String(text) => !text.trim().is_empty(),
            serde_json::Value::Array(items) => !items.is_empty(),
            serde_json::Value::Object(map) => !map.is_empty(),
            serde_json::Value::Null => false,
            _ => true,
        },
        _ => false,
    })
}

fn op_kind_name(op: &crate::planner::PlanV2Op) -> &'static str {
    match op {
        crate::planner::PlanV2Op::Fetch { .. } => "fetch",
        crate::planner::PlanV2Op::Aggregate { .. } => "aggregate",
        crate::planner::PlanV2Op::Compare { .. } => "compare",
        crate::planner::PlanV2Op::FilterRows { .. } => "filter_rows",
        crate::planner::PlanV2Op::Rank { .. } => "rank",
        crate::planner::PlanV2Op::DistanceHaversine { .. } => "distance_haversine",
        crate::planner::PlanV2Op::JoinOnTime { .. } => "join_on_time",
        crate::planner::PlanV2Op::ThresholdCheck { .. } => "threshold_check",
        crate::planner::PlanV2Op::TrendSummary { .. } => "trend_summary",
    }
}

fn operation_user_label(kind: &str) -> &'static str {
    match kind {
        "aggregate" => "an aggregation",
        "compare" => "a comparison",
        "distance_haversine" => "a distance calculation",
        "join_on_time" => "a time-based join",
        "threshold_check" => "a threshold check",
        "trend_summary" => "a trend summary",
        _ => "a required operation",
    }
}

fn deterministic_answer_is_final(kind: &DeterministicAnswerKind) -> bool {
    matches!(
        kind,
        DeterministicAnswerKind::RowList | DeterministicAnswerKind::Diagnostic
    )
}

fn operation_is_semantically_required(kind: &str) -> bool {
    matches!(
        kind,
        "aggregate"
            | "compare"
            | "distance_haversine"
            | "join_on_time"
            | "threshold_check"
            | "trend_summary"
    )
}

fn required_operation_kinds(plan: &crate::planner::PlanV2) -> BTreeSet<&'static str> {
    plan.steps
        .iter()
        .map(|step| op_kind_name(&step.op))
        .filter(|kind| operation_is_semantically_required(kind))
        .collect()
}

fn normalized_filter_rows_operator(op: &str) -> Option<&'static str> {
    match op.trim().to_ascii_lowercase().as_str() {
        "eq" => Some("eq"),
        "ne" => Some("ne"),
        "contains" => Some("contains"),
        "gt" => Some("gt"),
        "gte" | "ge" => Some("ge"),
        "lt" => Some("lt"),
        "lte" | "le" => Some("le"),
        _ => None,
    }
}

fn collect_filter_rows_compatible_predicates(
    value: &serde_json::Value,
    path: String,
    out: &mut Vec<ScopeConstraint>,
) -> bool {
    match value {
        serde_json::Value::Object(map) => {
            for (key, nested) in map {
                let key_lower = key.to_ascii_lowercase();
                match key_lower.as_str() {
                    "and" => match nested {
                        serde_json::Value::Array(items) => {
                            for item in items {
                                if !collect_filter_rows_compatible_predicates(
                                    item,
                                    path.clone(),
                                    out,
                                ) {
                                    return false;
                                }
                            }
                        }
                        _ => {
                            if !collect_filter_rows_compatible_predicates(nested, path.clone(), out)
                            {
                                return false;
                            }
                        }
                    },
                    "or" | "not" | "in" | "between" | "like" => return false,
                    _ => {
                        if let Some(op) = normalized_filter_rows_operator(&key_lower) {
                            if path.is_empty() {
                                return false;
                            }
                            let values = filter_rows_value_strings(nested);
                            if values.is_empty() {
                                return false;
                            }
                            out.push(ScopeConstraint {
                                root: String::new(),
                                field: path.clone(),
                                op: Some(op.to_string()),
                                values,
                            });
                            continue;
                        }
                        let new_path = if path.is_empty() {
                            key.clone()
                        } else {
                            format!("{path}.{key}")
                        };
                        if !collect_filter_rows_compatible_predicates(nested, new_path, out) {
                            return false;
                        }
                    }
                }
            }
            true
        }
        serde_json::Value::String(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::Bool(_) => {
            if path.is_empty() {
                return false;
            }
            let values = filter_rows_value_strings(value);
            if values.is_empty() {
                return false;
            }
            out.push(ScopeConstraint {
                root: String::new(),
                field: path,
                op: Some("eq".to_string()),
                values,
            });
            true
        }
        _ => false,
    }
}

fn collect_plan_scope_constraints(plan: &crate::planner::PlanV2) -> Vec<ScopeConstraint> {
    let steps_by_id = plan
        .steps
        .iter()
        .map(|step| (step.id.as_str(), step))
        .collect::<HashMap<_, _>>();

    fn step_root(
        step_id: &str,
        steps_by_id: &HashMap<&str, &crate::planner::PlanV2Step>,
    ) -> Option<String> {
        let step = steps_by_id.get(step_id)?;
        match &step.op {
            crate::planner::PlanV2Op::Fetch { root_field, .. } => Some(root_field.clone()),
            crate::planner::PlanV2Op::Aggregate { source, .. }
            | crate::planner::PlanV2Op::FilterRows { source, .. }
            | crate::planner::PlanV2Op::Rank { source, .. }
            | crate::planner::PlanV2Op::ThresholdCheck { source, .. }
            | crate::planner::PlanV2Op::TrendSummary { source, .. } => {
                step_root(source, steps_by_id)
            }
            crate::planner::PlanV2Op::Compare { left, .. } => step_root(left, steps_by_id),
            crate::planner::PlanV2Op::DistanceHaversine { .. }
            | crate::planner::PlanV2Op::JoinOnTime { .. } => None,
        }
    }

    let mut out = Vec::new();
    for step in &plan.steps {
        match &step.op {
            crate::planner::PlanV2Op::Fetch {
                root_field, filter, ..
            } => {
                let Some(filter) = filter else {
                    continue;
                };
                let mut constraints = Vec::new();
                if collect_filter_rows_compatible_predicates(
                    filter,
                    String::new(),
                    &mut constraints,
                ) {
                    for mut constraint in constraints {
                        constraint.root = root_field.clone();
                        out.push(constraint);
                    }
                }
            }
            crate::planner::PlanV2Op::FilterRows {
                source,
                field,
                operator,
                value,
            } => {
                let values = filter_rows_value_strings(value);
                let Some(normalized_op) = normalized_filter_rows_operator(operator) else {
                    continue;
                };
                if values.is_empty() {
                    continue;
                }
                let Some(root) = step_root(source, &steps_by_id) else {
                    continue;
                };
                out.push(ScopeConstraint {
                    root,
                    field: field.clone(),
                    op: Some(normalized_op.to_string()),
                    values,
                });
            }
            _ => {}
        }
    }
    out.sort_by(|a, b| {
        a.root
            .cmp(&b.root)
            .then_with(|| a.field.cmp(&b.field))
            .then_with(|| a.op.cmp(&b.op))
            .then_with(|| a.values.cmp(&b.values))
    });
    out.dedup_by(|a, b| {
        a.root == b.root && a.field == b.field && a.op == b.op && a.values == b.values
    });
    out
}

fn restore_simple_post_fetch_filters(
    original: &crate::planner::PlanV2,
    repaired: &mut crate::planner::PlanV2,
    _schema_registry: &crate::schema_registry::SchemaRegistry,
) -> bool {
    if original.steps.len() != 1 || repaired.steps.len() != 1 {
        return false;
    }

    let crate::planner::PlanV2Op::Fetch {
        root_field: original_root,
        fields: _,
        filter: Some(original_filter),
        ..
    } = &original.steps[0].op
    else {
        return false;
    };

    let mut predicates = Vec::new();
    if !collect_filter_rows_compatible_predicates(original_filter, String::new(), &mut predicates) {
        return false;
    }
    if predicates.is_empty() {
        return false;
    }

    let fetch_id = repaired.steps[0].id.clone();
    {
        let crate::planner::PlanV2Op::Fetch {
            root_field,
            fields,
            filter,
            ..
        } = &mut repaired.steps[0].op
        else {
            return false;
        };

        if root_field != original_root || filter.is_some() {
            return false;
        }

        for predicate in &predicates {
            if !fields.iter().any(|field| field == &predicate.field) {
                fields.push(predicate.field.clone());
            }
        }
    }

    let mut restored_any = false;
    let mut source = fetch_id;
    for (next_idx, predicate) in (repaired.steps.len() + 1..).zip(predicates) {
        let step_id = format!("s{next_idx}");
        repaired.steps.push(crate::planner::PlanV2Step {
            id: step_id.clone(),
            op: crate::planner::PlanV2Op::FilterRows {
                source: source.clone(),
                field: predicate.field.clone(),
                operator: predicate.op.unwrap_or_else(|| "eq".to_string()),
                value: predicate
                    .values
                    .first()
                    .map(|value| serde_json::json!(value))
                    .unwrap_or(serde_json::Value::Null),
            },
        });
        source = step_id;
        restored_any = true;
    }

    if restored_any {
        repaired.notes.push(
            "Preserved unsupported fetch filter constraints as local filter_rows steps."
                .to_string(),
        );
    }

    restored_any
}

fn semantic_repair_guard_message(
    original: &crate::planner::PlanV2,
    repaired: &crate::planner::PlanV2,
) -> Option<String> {
    let original_ops = required_operation_kinds(original);
    let repaired_ops = repaired
        .steps
        .iter()
        .map(|step| op_kind_name(&step.op))
        .collect::<BTreeSet<_>>();
    let dropped_ops = original_ops
        .into_iter()
        .filter(|kind| !repaired_ops.contains(kind))
        .collect::<Vec<_>>();
    if dropped_ops.is_empty() {
        let original_constraints = collect_plan_scope_constraints(original);
        if original_constraints.is_empty() {
            return None;
        }
        let repaired_constraints = collect_plan_scope_constraints(repaired);
        let missing_constraints = original_constraints
            .into_iter()
            .filter(|constraint| {
                !repaired_constraints.iter().any(|candidate| {
                    candidate.root == constraint.root
                        && candidate.field == constraint.field
                        && candidate.op == constraint.op
                        && candidate.values == constraint.values
                })
            })
            .collect::<Vec<_>>();
        if missing_constraints.is_empty() {
            return None;
        }
        let rendered = missing_constraints
            .iter()
            .map(format_scope_constraint)
            .collect::<Vec<_>>()
            .join(", ");
        return Some(format!(
            "I can’t answer this request because the repair dropped required filter condition(s): {rendered}. The available schema/backend could not preserve that scope without changing the meaning."
        ));
    }
    let dropped_labels = dropped_ops
        .iter()
        .map(|kind| operation_user_label(kind))
        .collect::<Vec<_>>();
    Some(format!(
        "I can’t answer this request because it requires {} and the available schema/backend could not support that request without changing its meaning.",
        dropped_labels.join(", ")
    ))
}

fn deterministic_fallback_guard_message(plans: &[&crate::planner::PlanV2]) -> Option<String> {
    let required_ops = plans
        .iter()
        .flat_map(|plan| required_operation_kinds(plan).into_iter())
        .collect::<BTreeSet<_>>();
    if required_ops.is_empty() {
        return None;
    }
    let required_labels = required_ops
        .iter()
        .map(|kind| operation_user_label(kind))
        .collect::<Vec<_>>();
    Some(format!(
        "I can’t answer this request because it requires {} and the available schema/backend could not produce a valid executable plan for it. A simple fetch fallback would change the meaning of the request.",
        required_labels.join(", ")
    ))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct GroundingConfidenceSummary {
    overall: &'static str,
    clarification_recommended: bool,
    grounded_mentions: Vec<String>,
    schema_candidate_mentions: Vec<String>,
    ambiguous_mentions: Vec<String>,
    unresolved_mentions: Vec<String>,
    mentions_requiring_confirmation: Vec<String>,
    signals: Vec<String>,
}

fn summarize_grounding_confidence(
    entity_resolutions: &[crate::entity_linker::EntityResolution],
) -> GroundingConfidenceSummary {
    if entity_resolutions.is_empty() {
        return GroundingConfidenceSummary {
            overall: "none",
            clarification_recommended: false,
            grounded_mentions: Vec::new(),
            schema_candidate_mentions: Vec::new(),
            ambiguous_mentions: Vec::new(),
            unresolved_mentions: Vec::new(),
            mentions_requiring_confirmation: Vec::new(),
            signals: vec!["no_entity_grounding_needed".to_string()],
        };
    }

    let grounded_mentions = entity_resolutions
        .iter()
        .filter(|resolution| matches!(resolution.status, ResolutionStatus::Grounded))
        .map(|resolution| resolution.mention.clone())
        .collect::<Vec<_>>();
    let schema_candidate_mentions = entity_resolutions
        .iter()
        .filter(|resolution| matches!(resolution.status, ResolutionStatus::SchemaCandidate))
        .map(|resolution| resolution.mention.clone())
        .collect::<Vec<_>>();
    let ambiguous_mentions = entity_resolutions
        .iter()
        .filter(|resolution| matches!(resolution.status, ResolutionStatus::Ambiguous))
        .map(|resolution| resolution.mention.clone())
        .collect::<Vec<_>>();
    let unresolved_mentions = entity_resolutions
        .iter()
        .filter(|resolution| matches!(resolution.status, ResolutionStatus::Unresolved))
        .map(|resolution| resolution.mention.clone())
        .collect::<Vec<_>>();

    let overall = if !ambiguous_mentions.is_empty() {
        "clarification_needed"
    } else if !unresolved_mentions.is_empty() && grounded_mentions.is_empty() {
        "low_confidence"
    } else if !schema_candidate_mentions.is_empty() && grounded_mentions.is_empty() {
        "needs_confirmation"
    } else if !unresolved_mentions.is_empty() || !schema_candidate_mentions.is_empty() {
        "mixed"
    } else {
        "high_confidence"
    };

    let clarification_recommended =
        matches!(overall, "clarification_needed" | "needs_confirmation");
    let mut signals = Vec::new();
    if !grounded_mentions.is_empty() {
        signals.push("grounded_entity_match".to_string());
    }
    if !schema_candidate_mentions.is_empty() {
        signals.push("schema_candidate_only".to_string());
    }
    if !ambiguous_mentions.is_empty() {
        signals.push("schema_or_execution_ambiguity".to_string());
    }
    if !unresolved_mentions.is_empty() {
        signals.push("unresolved_entity_lookup".to_string());
    }

    let mentions_requiring_confirmation = ambiguous_mentions
        .iter()
        .chain(schema_candidate_mentions.iter())
        .cloned()
        .collect::<Vec<_>>();

    GroundingConfidenceSummary {
        overall,
        clarification_recommended,
        grounded_mentions,
        schema_candidate_mentions,
        ambiguous_mentions,
        unresolved_mentions,
        mentions_requiring_confirmation,
        signals,
    }
}

fn humanize_entity_family(family_type: &str) -> String {
    let mut rendered = String::new();
    let mut prev_was_lower_or_digit = false;
    for ch in family_type.chars() {
        let is_upper = ch.is_ascii_uppercase();
        if is_upper && prev_was_lower_or_digit && !rendered.is_empty() {
            rendered.push(' ');
        }
        rendered.push(ch.to_ascii_lowercase());
        prev_was_lower_or_digit = ch.is_ascii_lowercase() || ch.is_ascii_digit();
    }
    rendered
}

fn join_human_list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [first, second] => format!("{first} or {second}"),
        _ => {
            let mut rendered = items[..items.len() - 1].join(", ");
            rendered.push_str(", or ");
            rendered.push_str(&items[items.len() - 1]);
            rendered
        }
    }
}

fn clarification_options_for_resolution(
    resolution: &crate::entity_linker::EntityResolution,
) -> Vec<String> {
    let grounded_options = resolution
        .grounded_matches
        .iter()
        .map(|grounded| {
            let label = grounded
                .display_label
                .as_deref()
                .unwrap_or(&grounded.canonical_value);
            format!(
                "`{label}` ({})",
                humanize_entity_family(&grounded.family_type)
            )
        })
        .collect::<BTreeSet<_>>();
    if !grounded_options.is_empty() {
        return grounded_options.into_iter().collect::<Vec<_>>();
    }

    resolution
        .schema_candidates
        .iter()
        .map(|candidate| humanize_entity_family(&candidate.family_type))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|family| format!("a {family}"))
        .collect::<Vec<_>>()
}

fn mention_has_prefixed_digits(mention: &str, prefix: &str) -> bool {
    let lower = mention.trim().to_ascii_lowercase();
    lower.starts_with(prefix)
        && lower.len() > prefix.len()
        && lower[prefix.len()..].chars().all(|ch| ch.is_ascii_digit())
}

fn normalized_alnum_lower(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .map(|ch| ch.to_ascii_lowercase())
        .collect()
}

fn alias_looks_like_abbreviation(alias: &str) -> bool {
    let trimmed = alias.trim();
    trimmed.len() <= 4
        && trimmed
            .chars()
            .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit())
}

fn concept_alias_prefixes(aliases: &[String]) -> Vec<String> {
    let mut prefixes = Vec::new();
    for alias in aliases {
        let trimmed = alias.trim();
        if trimmed.is_empty() {
            continue;
        }

        let normalized = normalized_alnum_lower(trimmed);
        if (2..=5).contains(&normalized.len())
            && trimmed.chars().any(|ch| ch.is_ascii_uppercase())
            && trimmed.chars().all(|ch| ch.is_ascii_alphanumeric())
        {
            prefixes.push(normalized.clone());
        }

        let initials = trimmed
            .split(|ch: char| !ch.is_ascii_alphanumeric())
            .filter(|token| !token.is_empty())
            .filter_map(|token| token.chars().next())
            .collect::<String>()
            .to_ascii_lowercase();
        if initials.len() >= 2 {
            prefixes.push(initials);
        }
    }

    prefixes.sort();
    prefixes.dedup();
    prefixes
}

fn schema_candidate_has_strong_type_hint(
    schema_registry: &SchemaRegistry,
    user_message: &str,
    resolution: &crate::entity_linker::EntityResolution,
) -> bool {
    if !matches!(resolution.status, ResolutionStatus::SchemaCandidate)
        || resolution.schema_candidates.len() != 1
    {
        return false;
    }

    let candidate = &resolution.schema_candidates[0];
    let family_type = candidate.family_type.as_str();
    let lower_message = user_message.to_ascii_lowercase();
    let lower_mention = resolution.mention.trim().to_ascii_lowercase();
    let aliases = schema_registry.concept_aliases_for_type(family_type);
    let alias_phrase_match = aliases.iter().any(|alias| {
        let phrase = alias.trim().to_ascii_lowercase();
        !phrase.is_empty()
            && !alias_looks_like_abbreviation(alias)
            && (lower_message.contains(&phrase) || lower_mention.contains(&phrase))
    });
    if alias_phrase_match {
        return true;
    }

    let prefix_match = concept_alias_prefixes(&aliases)
        .iter()
        .any(|prefix| mention_has_prefixed_digits(&resolution.mention, prefix));
    if prefix_match {
        return true;
    }

    let normalized_message = normalized_alnum_lower(user_message);
    candidate
        .key_fields
        .iter()
        .chain(candidate.label_fields.iter())
        .chain(candidate.filter_fields.iter())
        .map(|field| normalized_alnum_lower(field))
        .filter(|field| !field.is_empty())
        .any(|field| normalized_message.contains(&field))
}

fn selectable_scalar_field(
    schema_registry: &SchemaRegistry,
    root_field: &str,
    field: &str,
) -> bool {
    if field.contains('.') {
        return false;
    }
    let Some(return_type) = schema_registry.query_return_type(root_field) else {
        return false;
    };
    let Some(field_type) = schema_registry.object_field_type(return_type, field) else {
        return false;
    };
    schema_registry.object_field_names(field_type).is_none()
}

fn append_rank_source_display_fields(
    plan: &mut crate::planner::PlanV2,
    schema_registry: &SchemaRegistry,
) {
    let rank_sources = plan
        .steps
        .iter()
        .filter_map(|step| match &step.op {
            crate::planner::PlanV2Op::Rank { source, .. } => Some(source.clone()),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    if rank_sources.is_empty() {
        return;
    }

    let mut added = Vec::new();
    for step in &mut plan.steps {
        if !rank_sources.contains(&step.id) {
            continue;
        }
        let crate::planner::PlanV2Op::Fetch {
            root_field, fields, ..
        } = &mut step.op
        else {
            continue;
        };
        let roles = schema_registry.field_roles_for_root(root_field);
        for field in roles
            .label_fields
            .iter()
            .chain(roles.entity_key_fields.iter())
        {
            if fields.iter().any(|existing| existing == field)
                || !selectable_scalar_field(schema_registry, root_field, field)
            {
                continue;
            }
            fields.push(field.clone());
            added.push(format!("{}.{}", step.id, field));
            if added.len() >= 12 {
                break;
            }
        }
    }

    if !added.is_empty() {
        plan.notes.push(format!(
            "rank_source_display_fields_added: {}",
            added.join(", ")
        ));
    }
}

fn promote_strong_single_backend_candidates(
    schema_registry: &SchemaRegistry,
    user_message: &str,
    entity_resolutions: &mut [crate::entity_linker::EntityResolution],
) {
    for resolution in entity_resolutions {
        if !matches!(resolution.status, ResolutionStatus::SchemaCandidate)
            || resolution.grounded_matches.len() != 1
            || !schema_candidate_has_strong_type_hint(schema_registry, user_message, resolution)
        {
            continue;
        }

        let grounded = &resolution.grounded_matches[0];
        if grounded.stable_key_field.is_some() && grounded.stable_key_value.is_some() {
            resolution.status = ResolutionStatus::Grounded;
            resolution
                .notes
                .retain(|note| !note.contains("confirmation is required"));
            resolution.notes.push(
                "Promoted single exact backend match to grounded because the prompt strongly typed the entity and a stable key was available; confirmation is no longer required."
                    .to_string(),
            );
        }
    }
}

fn build_grounding_clarification_message(
    schema_registry: &SchemaRegistry,
    user_message: &str,
    entity_resolutions: &[crate::entity_linker::EntityResolution],
) -> Option<String> {
    let summary = summarize_grounding_confidence(entity_resolutions);
    if !matches!(
        summary.overall,
        "clarification_needed" | "needs_confirmation"
    ) {
        return None;
    }

    let relevant_resolutions = entity_resolutions
        .iter()
        .filter(|resolution| match resolution.status {
            ResolutionStatus::Ambiguous => true,
            ResolutionStatus::SchemaCandidate => {
                !resolution.grounded_matches.is_empty()
                    || !schema_candidate_has_strong_type_hint(
                        schema_registry,
                        user_message,
                        resolution,
                    )
            }
            _ => false,
        })
        .collect::<Vec<_>>();

    if relevant_resolutions.is_empty() {
        return None;
    }

    let details = relevant_resolutions
        .iter()
        .map(|resolution| {
            let options = clarification_options_for_resolution(resolution);
            match resolution.status {
                ResolutionStatus::Ambiguous => {
                    if options.is_empty() {
                        format!("`{}` has multiple possible matches", resolution.mention)
                    } else {
                        format!(
                            "`{}` could refer to {}",
                            resolution.mention,
                            join_human_list(&options)
                        )
                    }
                }
                ResolutionStatus::SchemaCandidate => {
                    if options.is_empty() {
                        format!("`{}` needs a more specific identifier", resolution.mention)
                    } else {
                        format!(
                            "`{}` looks like {}",
                            resolution.mention,
                            join_human_list(&options)
                        )
                    }
                }
                _ => unreachable!("filtered to ambiguous/schema-candidate resolutions"),
            }
        })
        .collect::<Vec<_>>();

    let has_ambiguous_resolution = relevant_resolutions
        .iter()
        .any(|resolution| matches!(resolution.status, ResolutionStatus::Ambiguous));

    let detail_text = if details.is_empty() {
        "I need a bit more detail about the entity you mean.".to_string()
    } else {
        details.join("; ")
    };

    Some(if has_ambiguous_resolution {
        format!(
            "I found multiple possible entity matches. {detail_text}. Please clarify which one you mean before I run the query."
        )
    } else {
        format!(
            "I found likely entity candidates, but not enough grounding to choose confidently. {detail_text}. Please clarify which entity you mean before I run the query."
        )
    })
}

fn push_unique_query_root(roots: &mut Vec<String>, root: &str) {
    if !root.starts_with("query") {
        return;
    }
    if roots.iter().any(|existing| existing == root) {
        return;
    }
    roots.push(root.to_string());
}

fn anchored_query_roots(
    schema_registry: &SchemaRegistry,
    entity_resolutions: &[crate::entity_linker::EntityResolution],
) -> Vec<String> {
    let mut direct_roots = Vec::new();
    for resolution in entity_resolutions {
        match resolution.status {
            ResolutionStatus::Grounded => {
                for grounded in &resolution.grounded_matches {
                    push_unique_query_root(&mut direct_roots, &grounded.root_field);
                    for root in schema_registry.query_roots_for_type(&grounded.family_type) {
                        push_unique_query_root(&mut direct_roots, &root);
                    }
                }
            }
            ResolutionStatus::SchemaCandidate => {
                for candidate in &resolution.schema_candidates {
                    for root in &candidate.lookup_roots {
                        push_unique_query_root(&mut direct_roots, root);
                    }
                    for root in schema_registry.query_roots_for_type(&candidate.family_type) {
                        push_unique_query_root(&mut direct_roots, &root);
                    }
                }
            }
            ResolutionStatus::Ambiguous | ResolutionStatus::Unresolved => {}
        }
    }

    if direct_roots.is_empty() {
        return Vec::new();
    }

    let mut anchored_roots = direct_roots.clone();
    for root in direct_roots {
        for neighbor in schema_registry.relation_neighbor_query_roots(&root) {
            push_unique_query_root(&mut anchored_roots, &neighbor);
        }
    }
    anchored_roots
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ContainsCompareBranch {
    fetch_id: String,
    root_field: String,
    field: String,
    value: String,
}

fn compare_branch_from_step(
    step_id: &str,
    steps_by_id: &HashMap<&str, &crate::planner::PlanV2Step>,
) -> Option<ContainsCompareBranch> {
    let step = steps_by_id.get(step_id)?;
    match &step.op {
        crate::planner::PlanV2Op::FilterRows {
            source,
            field,
            operator,
            value,
        } => {
            if !(operator.eq_ignore_ascii_case("contains") || operator.eq_ignore_ascii_case("like"))
            {
                return None;
            }
            let value_str = value.as_str()?.trim();
            if value_str.is_empty() {
                return None;
            }
            let fetch_step = steps_by_id.get(source.as_str())?;
            let crate::planner::PlanV2Op::Fetch {
                root_field, filter, ..
            } = &fetch_step.op
            else {
                return None;
            };
            if filter.is_some() {
                return None;
            }
            Some(ContainsCompareBranch {
                fetch_id: source.clone(),
                root_field: root_field.clone(),
                field: field.clone(),
                value: value_str.to_string(),
            })
        }
        crate::planner::PlanV2Op::Aggregate { source, .. } => {
            compare_branch_from_step(source, steps_by_id)
        }
        _ => None,
    }
}

fn compare_contains_antipattern_error(
    plan: &crate::planner::PlanV2,
    schema_registry: &SchemaRegistry,
    entity_mentions: &[String],
) -> Option<String> {
    if entity_mentions.len() < 2 {
        return None;
    }
    let steps_by_id = plan
        .steps
        .iter()
        .map(|step| (step.id.as_str(), step))
        .collect::<HashMap<_, _>>();

    for step in &plan.steps {
        let crate::planner::PlanV2Op::Compare { left, right, .. } = &step.op else {
            continue;
        };
        let Some(left_branch) = compare_branch_from_step(left, &steps_by_id) else {
            continue;
        };
        let Some(right_branch) = compare_branch_from_step(right, &steps_by_id) else {
            continue;
        };
        if left_branch.fetch_id != right_branch.fetch_id
            || !left_branch
                .root_field
                .eq_ignore_ascii_case(&right_branch.root_field)
            || !left_branch.field.eq_ignore_ascii_case(&right_branch.field)
            || left_branch.value.eq_ignore_ascii_case(&right_branch.value)
        {
            continue;
        }
        let roles = schema_registry.field_roles_for_root(&left_branch.root_field);
        let field_is_label_like = roles
            .label_fields
            .iter()
            .any(|field| field.eq_ignore_ascii_case(&left_branch.field))
            || {
                let lower = left_branch.field.to_ascii_lowercase();
                lower.contains("name") || lower.contains("label") || lower.contains("title")
            };
        if !field_is_label_like {
            continue;
        }
        let mentions_cover_values = entity_mentions.iter().any(|mention| {
            let lower = mention.to_ascii_lowercase();
            lower.contains(&left_branch.value.to_ascii_lowercase())
        }) && entity_mentions.iter().any(|mention| {
            let lower = mention.to_ascii_lowercase();
            lower.contains(&right_branch.value.to_ascii_lowercase())
        });
        if !mentions_cover_values {
            continue;
        }
        return Some(format!(
            "compare step '{}' derives both branches from one broad `{}` fetch and post-filters `{}` with contains-style fragments (`{}` / `{}`). Use per-entity scoped fetches or grounded exact filters instead.",
            step.id,
            left_branch.root_field,
            left_branch.field,
            left_branch.value,
            right_branch.value
        ));
    }
    None
}

pub(crate) async fn run_ir_pipeline(
    state: &AppState,
    model_name: &str,
    user_message: &str,
    execute: bool,
    debug_output: bool,
) -> PipelineResult<String> {
    run_ir_pipeline_with_progress(state, model_name, user_message, execute, debug_output, None)
        .await
}

pub(crate) async fn run_ir_pipeline_with_progress(
    state: &AppState,
    model_name: &str,
    user_message: &str,
    execute: bool,
    debug_output: bool,
    progress: Option<ProgressCallback<'_>>,
) -> PipelineResult<String> {
    let pipeline_started = Instant::now();
    emit_progress(
        progress,
        PipelineProgressEvent::stage(
            "grounding",
            "running",
            "Resolving entity mentions and schema anchors.",
        ),
    );
    let schema_registry = state.schema_registry.read().await.clone();
    let schema_version = state.schema_meta.read().await.loaded_at.to_rfc3339();
    let provider_profile = prompt_cache_profile(infer_provider_kind(&state.config, model_name));
    let retrieval_budget = schema_registry.planner_retrieval_budget(user_message);
    let today_utc = Utc::now().date_naive().to_string();
    let static_hints = if let Some(hints) = state
        .planner_cache
        .read()
        .await
        .static_hints(&schema_version)
    {
        hints
    } else {
        let root_fields_vec = schema_registry
            .root_fields()
            .into_iter()
            .filter(|f| f.starts_with("query"))
            .collect::<Vec<_>>();
        let hints = StaticPromptHints {
            root_fields: root_fields_vec.join(", "),
            policy_hints: policy_hints_for_prompt(state.sls.as_ref()),
            join_hints: state
                .sls
                .as_ref()
                .map(|s| s.join_paths_prompt_block())
                .unwrap_or_default(),
            metric_hints: state
                .sls
                .as_ref()
                .map(|s| s.metrics_prompt_block())
                .unwrap_or_default(),
            field_hints: state
                .sls
                .as_ref()
                .map(|s| {
                    format!(
                        "{}{}",
                        s.intent_vocabulary_prompt_block(),
                        s.canonical_fields_prompt_block()
                    )
                })
                .unwrap_or_default(),
        };
        state
            .planner_cache
            .write()
            .await
            .set_static_hints(schema_version.clone(), hints.clone());
        hints
    };
    let planner_examples = planner_examples_for_message(user_message);
    let entity_mentions = extracted_entity_mentions(&schema_registry, user_message);
    let entity_mention_hints = render_entity_mention_hints_block(&schema_registry, user_message);
    let mut entity_resolutions = if execute {
        resolve_grounded_entity_resolutions(
            state,
            &schema_registry,
            user_message,
            retrieval_budget.entity_resolution_limit,
        )
        .await
    } else {
        resolve_entity_resolutions(
            &schema_registry,
            user_message,
            retrieval_budget.entity_resolution_limit,
        )
    };
    promote_strong_single_backend_candidates(
        &schema_registry,
        user_message,
        &mut entity_resolutions,
    );
    let entity_resolution_hints =
        render_entity_resolution_block_from_resolutions(&entity_resolutions);
    emit_progress(
        progress,
        PipelineProgressEvent::stage(
            "grounding",
            "completed",
            format!("Resolved {} entity mention(s).", entity_resolutions.len()),
        ),
    );
    if execute
        && let Some(clarification) = build_grounding_clarification_message(
            &schema_registry,
            user_message,
            &entity_resolutions,
        )
    {
        if debug_output {
            let grounding_confidence = build_grounding_confidence_signal(&entity_resolutions);
            return Ok(format!(
                "{}\n\n{}\n\ngrounding_confidence:\n```json\n{}\n```\n\nFinal Answer:\n{}",
                entity_mention_hints,
                entity_resolution_hints,
                serde_json::to_string_pretty(&grounding_confidence).unwrap_or_default(),
                clarification
            ));
        }
        return Ok(clarification);
    }
    let preferred_root_fields_vec = anchored_query_roots(&schema_registry, &entity_resolutions);
    let context_cache_key = PlannerContextCacheKey::new(
        schema_version.clone(),
        user_message,
        retrieval_budget.root_limit,
        retrieval_budget.field_limit,
        preferred_root_fields_vec.clone(),
    );
    let context_entry = if let Some(entry) = state
        .planner_cache
        .read()
        .await
        .context_entry(&context_cache_key)
    {
        entry
    } else {
        let (schema_snippet, schema_retrieval, preferred_root_fields) = if preferred_root_fields_vec
            .is_empty()
        {
            let raw_root_limit = (retrieval_budget.root_limit * 2)
                .max(retrieval_budget.root_limit + 2)
                .min(12);
            let raw_slice = schema_registry.schema_retrieval_slice(
                user_message,
                raw_root_limit,
                retrieval_budget.field_limit,
            );
            let capability_graph =
                CapabilityGraph::from_registry(&schema_registry, state.sls.as_ref());
            let slice = capability_graph.refine_retrieval_slice(
                raw_slice,
                user_message,
                retrieval_budget.root_limit,
            );
            let refined_root_fields = slice
                .roots
                .iter()
                .take(retrieval_budget.root_limit)
                .map(|root| root.root.clone())
                .collect::<Vec<_>>();
            let snippet = schema_registry.planner_context_from_slice(&slice);
            let retrieval = schema_retrieval_summary(&slice, retrieval_budget);
            let preferred = if refined_root_fields.is_empty() {
                schema_registry.best_matching_query_roots(user_message, retrieval_budget.root_limit)
            } else {
                refined_root_fields
            };
            (snippet, retrieval, preferred)
        } else {
            (
                schema_registry.anchored_planner_context(
                    user_message,
                    &preferred_root_fields_vec,
                    retrieval_budget.field_limit,
                ),
                anchored_schema_retrieval_summary(&preferred_root_fields_vec, retrieval_budget),
                preferred_root_fields_vec.clone(),
            )
        };
        let entry = PlannerContextCacheEntry {
            schema_snippet,
            preferred_root_fields,
            schema_retrieval,
        };
        state
            .planner_cache
            .write()
            .await
            .insert_context_entry(context_cache_key, entry.clone());
        entry
    };
    let schema_snippet = context_entry.schema_snippet;
    let schema_retrieval = context_entry.schema_retrieval;
    let preferred_root_fields = if context_entry.preferred_root_fields.is_empty() {
        "(none)".to_string()
    } else {
        context_entry.preferred_root_fields.join(", ")
    };
    let planner_prompt_cache_shape = estimate_planner_prompt_cache_shape(
        &today_utc,
        &static_hints.root_fields,
        &preferred_root_fields,
        &planner_examples,
        &entity_mention_hints,
        &entity_resolution_hints,
        &static_hints.policy_hints,
        &static_hints.join_hints,
        &static_hints.metric_hints,
        &static_hints.field_hints,
        &schema_snippet,
        user_message,
    );
    let ir_prompt = build_planner_prompt(&PlannerPromptContext {
        today_utc: &today_utc,
        root_fields: &static_hints.root_fields,
        preferred_root_fields: &preferred_root_fields,
        planner_examples: &planner_examples,
        entity_mention_hints: &entity_mention_hints,
        entity_resolution_hints: &entity_resolution_hints,
        policy_hints: &static_hints.policy_hints,
        join_hints: &static_hints.join_hints,
        metric_hints: &static_hints.metric_hints,
        field_hints: &static_hints.field_hints,
        schema_snippet: &schema_snippet,
        user_message,
    });

    let ir_agent = if model_name.is_empty() || model_name == state.config.model {
        state.cached_ir_agent.clone()
    } else {
        create_ir_agent(&state.config, model_name)
            .await
            .map_err(|e| PipelineError::planning(e.to_string()))?
    };

    let planner_started = Instant::now();
    let mut provider_token_usage = ProviderTokenUsage::default();
    emit_progress(
        progress,
        PipelineProgressEvent::stage("planning", "running", "Building PlanV2 with the planner."),
    );
    let planner_model_key = if model_name.is_empty() {
        state.config.model.clone()
    } else {
        model_name.to_string()
    };
    let planner_cache_key =
        PlannerResponseCacheKey::new(schema_version.clone(), planner_model_key, &ir_prompt);
    let cached_planner_response = state
        .planner_cache
        .read()
        .await
        .planner_response(&planner_cache_key);
    let planner_cache_hit = cached_planner_response.is_some();
    let ir_response = if let Some(response) = cached_planner_response {
        emit_progress(
            progress,
            PipelineProgressEvent::stage(
                "planning",
                "completed",
                "Planner response reused from cache.",
            ),
        );
        response
    } else {
        let response = ir_agent
            .prompt_extended(&ir_prompt)
            .await
            .map_err(|e| PipelineError::planning(e.to_string()))?;
        provider_token_usage += ProviderTokenUsage::from_rig(response.total_usage);
        let response = response.output;
        state
            .planner_cache
            .write()
            .await
            .insert_planner_response(planner_cache_key, response.clone());
        response
    };
    let planner_ms = planner_started.elapsed().as_millis();
    if !planner_cache_hit {
        emit_progress(
            progress,
            PipelineProgressEvent::stage(
                "planning",
                "completed",
                format!("Planner returned a response in {planner_ms}ms."),
            ),
        );
    }

    let mut last_plan_error: Option<String> = None;
    let mut repair_response_text: Option<String> = None;
    let mut repair_plan_error: Option<String> = None;
    let mut repair_scope_guard: Option<String> = None;
    let mut fallback_scope_guard: Option<String> = None;
    let mut allow_deterministic_fallback = false;
    let raw_plan_json = extract_plan_v2_json_from_response(&ir_response);
    let mut raw_repair_plan_json: Option<String> = None;
    let mut planner_repair_ms: Option<u128> = None;
    let mut planner_repair_prompt_chars: Option<usize> = None;
    let mut planner_repair_prompt_tokens_est: Option<usize> = None;
    let mut planner_repair_cache_hit: Option<bool> = None;
    let parsed_initial_plan = parse_plan_v2_struct_from_response(&ir_response);
    let mut parsed_repair_plan: Option<crate::planner::PlanV2> = None;
    emit_progress(
        progress,
        PipelineProgressEvent::stage("validation", "running", "Validating the PlanV2 response."),
    );
    let mut multi_step = if let Some(mut plan) = parsed_initial_plan.clone() {
        if let Err(e) = resolve_sls_metric_refs(&mut plan, state.sls.as_ref()) {
            last_plan_error = Some(e);
            allow_deterministic_fallback = true;
            None
        } else if let Err(e) =
            validate_sls_metric_sources(&plan, &schema_registry, state.sls.as_ref())
        {
            last_plan_error = Some(e);
            allow_deterministic_fallback = true;
            None
        } else {
            apply_parent_relation_rewrite(
                &mut plan,
                user_message,
                &schema_registry,
                state.sls.as_ref(),
            );
            append_rank_source_display_fields(&mut plan, &schema_registry);
            match validate_plan_v2(&plan, &schema_registry) {
                Ok(()) => {
                    if let Some(issue) = compare_contains_antipattern_error(
                        &plan,
                        &schema_registry,
                        &entity_mentions,
                    ) {
                        last_plan_error = Some(issue);
                        allow_deterministic_fallback = true;
                        None
                    } else {
                        let policy_eval =
                            evaluate_plan_policies(&plan, state.sls.as_ref(), &schema_registry);
                        if policy_eval.violations.is_empty() {
                            let notes = apply_policy_fixes(&mut plan, &policy_eval.fixes);
                            plan.notes.extend(notes);
                            plan_v2_to_multistep(&plan)
                        } else {
                            last_plan_error = Some(policy_eval.violations.join(" | "));
                            allow_deterministic_fallback = true;
                            None
                        }
                    }
                }
                Err(e) => {
                    last_plan_error = Some(e.to_string());
                    allow_deterministic_fallback = true;
                    None
                }
            }
        }
    } else {
        allow_deterministic_fallback = true;
        None
    };

    if multi_step.is_none() {
        emit_progress(
            progress,
            PipelineProgressEvent::stage(
                "repair",
                "running",
                "Initial plan was not executable; requesting bounded repair.",
            ),
        );
        let repair_prompt = build_plan_repair_prompt(&PlanRepairPromptContext {
            root_fields: &static_hints.root_fields,
            preferred_root_fields: &preferred_root_fields,
            today_utc: &today_utc,
            entity_mention_hints: &entity_mention_hints,
            entity_resolution_hints: &entity_resolution_hints,
            schema_snippet: &schema_snippet,
            policy_hints: &static_hints.policy_hints,
            join_hints: &static_hints.join_hints,
            metric_hints: &static_hints.metric_hints,
            field_hints: &static_hints.field_hints,
            previous_error: last_plan_error.as_deref().unwrap_or("none"),
            input: &ir_response,
        });
        planner_repair_prompt_chars = Some(repair_prompt.len());
        planner_repair_prompt_tokens_est = Some(estimate_tokens(&repair_prompt));

        let repair_started = Instant::now();
        let repair_cache_key = PlannerResponseCacheKey::new(
            schema_version.clone(),
            if model_name.is_empty() {
                state.config.model.clone()
            } else {
                model_name.to_string()
            },
            &repair_prompt,
        );
        let cached_repair_response = state
            .planner_cache
            .read()
            .await
            .planner_response(&repair_cache_key);
        planner_repair_cache_hit = Some(cached_repair_response.is_some());
        let repair_response_result = if let Some(response) = cached_repair_response {
            Ok(response)
        } else {
            let response = ir_agent
                .prompt_extended(&repair_prompt)
                .await
                .map_err(|e| e.to_string())
                .map(|response| {
                    provider_token_usage += ProviderTokenUsage::from_rig(response.total_usage);
                    response.output
                });
            if let Ok(response) = &response {
                state
                    .planner_cache
                    .write()
                    .await
                    .insert_planner_response(repair_cache_key, response.clone());
            }
            response
        };
        match repair_response_result {
            Ok(repair_response) => {
                planner_repair_ms = Some(repair_started.elapsed().as_millis());
                repair_response_text = Some(repair_response.clone());
                raw_repair_plan_json = extract_plan_v2_json_from_response(&repair_response);
                if let Some(mut plan) = parse_plan_v2_struct_from_response(&repair_response) {
                    if let Some(original_plan) = parsed_initial_plan.as_ref() {
                        restore_simple_post_fetch_filters(
                            original_plan,
                            &mut plan,
                            &schema_registry,
                        );
                    }
                    parsed_repair_plan = Some(plan.clone());
                    if let Some(original_plan) = parsed_initial_plan.as_ref()
                        && let Some(message) = semantic_repair_guard_message(original_plan, &plan)
                    {
                        repair_scope_guard = Some(message);
                        allow_deterministic_fallback = false;
                    } else if let Err(e) = resolve_sls_metric_refs(&mut plan, state.sls.as_ref()) {
                        repair_plan_error = Some(e);
                    } else if let Err(e) =
                        validate_sls_metric_sources(&plan, &schema_registry, state.sls.as_ref())
                    {
                        repair_plan_error = Some(e);
                    } else {
                        append_rank_source_display_fields(&mut plan, &schema_registry);
                        match validate_plan_v2(&plan, &schema_registry) {
                            Ok(()) => {
                                if let Some(issue) = compare_contains_antipattern_error(
                                    &plan,
                                    &schema_registry,
                                    &entity_mentions,
                                ) {
                                    repair_plan_error = Some(issue);
                                    allow_deterministic_fallback = true;
                                } else {
                                    let policy_eval = evaluate_plan_policies(
                                        &plan,
                                        state.sls.as_ref(),
                                        &schema_registry,
                                    );
                                    if policy_eval.violations.is_empty() {
                                        let notes =
                                            apply_policy_fixes(&mut plan, &policy_eval.fixes);
                                        plan.notes.extend(notes);
                                        multi_step = plan_v2_to_multistep(&plan);
                                    } else {
                                        repair_plan_error =
                                            Some(policy_eval.violations.join(" | "));
                                        allow_deterministic_fallback = true;
                                    }
                                }
                            }
                            Err(e) => {
                                repair_plan_error = Some(e.to_string());
                                allow_deterministic_fallback = true;
                            }
                        }
                    }
                } else {
                    repair_plan_error =
                        Some("repair response did not parse as valid PlanV2 JSON".to_string());
                    allow_deterministic_fallback = true;
                }
            }
            Err(e) => {
                planner_repair_ms = Some(repair_started.elapsed().as_millis());
                repair_plan_error = Some(e);
                allow_deterministic_fallback = false;
            }
        }
        emit_progress(
            progress,
            PipelineProgressEvent::stage(
                "repair",
                if multi_step.is_some() {
                    "completed"
                } else {
                    "failed"
                },
                if multi_step.is_some() {
                    "Repair produced a valid executable plan."
                } else {
                    "Repair did not produce an executable plan."
                },
            ),
        );
    }

    if let Some(guarded) = repair_scope_guard {
        let raw_plan_block = raw_plan_json
            .as_deref()
            .map(|json| format!("Raw Planner JSON:\n```json\n{json}\n```"))
            .unwrap_or_else(|| {
                "Raw Planner JSON:\n```text\nplanner response did not parse as valid PlanV2 JSON\n```"
                    .to_string()
            });
        let raw_repair_block = raw_repair_plan_json
            .as_deref()
            .map(|json| format!("\n\nRaw Repair JSON:\n```json\n{json}\n```"))
            .unwrap_or_default();
        if debug_output {
            return Ok(format!(
                "{}\n\n{}\n\n{}{}\n\nFinal Answer:\n{}",
                entity_mention_hints,
                entity_resolution_hints,
                raw_plan_block,
                raw_repair_block,
                guarded
            ));
        }
        return Ok(guarded);
    }

    if multi_step.is_none() && allow_deterministic_fallback {
        let fallback_guard_inputs = parsed_initial_plan
            .iter()
            .chain(parsed_repair_plan.iter())
            .collect::<Vec<_>>();
        fallback_scope_guard = deterministic_fallback_guard_message(&fallback_guard_inputs);
        if fallback_scope_guard.is_some() {
            allow_deterministic_fallback = false;
        }
    }

    if let Some(guarded) = fallback_scope_guard {
        let raw_plan_block = raw_plan_json
            .as_deref()
            .map(|json| format!("Raw Planner JSON:\n```json\n{json}\n```"))
            .unwrap_or_else(|| {
                "Raw Planner JSON:\n```text\nplanner response did not parse as valid PlanV2 JSON\n```"
                    .to_string()
            });
        let raw_repair_block = raw_repair_plan_json
            .as_deref()
            .map(|json| format!("\n\nRaw Repair JSON:\n```json\n{json}\n```"))
            .unwrap_or_default();
        if debug_output {
            return Ok(format!(
                "{}\n\n{}\n\n{}{}\n\nFinal Answer:\n{}",
                entity_mention_hints,
                entity_resolution_hints,
                raw_plan_block,
                raw_repair_block,
                guarded
            ));
        }
        return Ok(guarded);
    }

    if multi_step.is_none()
        && allow_deterministic_fallback
        && let Some(mut plan) =
            synthesize_simple_fetch_plan(&schema_registry, user_message, state.sls.as_ref())
    {
        emit_progress(
            progress,
            PipelineProgressEvent::stage(
                "fallback",
                "running",
                "Trying a deterministic schema-aware fallback plan.",
            ),
        );
        let mut rewritten = plan.clone();
        let did_rewrite = apply_parent_relation_rewrite(
            &mut rewritten,
            user_message,
            &schema_registry,
            state.sls.as_ref(),
        );
        let mut plan = if did_rewrite {
            match validate_plan_v2(&rewritten, &schema_registry) {
                Ok(()) => rewritten,
                Err(e) => {
                    plan.notes
                        .push(format!("parent_relation_rewrite_skipped: {e}"));
                    plan
                }
            }
        } else {
            plan
        };
        append_rank_source_display_fields(&mut plan, &schema_registry);
        match validate_plan_v2(&plan, &schema_registry) {
            Ok(()) => {
                let policy_eval =
                    evaluate_plan_policies(&plan, state.sls.as_ref(), &schema_registry);
                if policy_eval.violations.is_empty() {
                    let notes = apply_policy_fixes(&mut plan, &policy_eval.fixes);
                    plan.notes.extend(notes);
                    multi_step = plan_v2_to_multistep(&plan);
                } else {
                    last_plan_error = Some(policy_eval.violations.join(" | "));
                }
            }
            Err(e) => {
                last_plan_error = Some(e.to_string());
            }
        }
        emit_progress(
            progress,
            PipelineProgressEvent::stage(
                "fallback",
                if multi_step.is_some() {
                    "completed"
                } else {
                    "failed"
                },
                if multi_step.is_some() {
                    "Deterministic fallback produced an executable plan."
                } else {
                    "Deterministic fallback did not produce an executable plan."
                },
            ),
        );
    }

    if let Some(plan) = multi_step {
        emit_progress(
            progress,
            PipelineProgressEvent::stage(
                "validation",
                "completed",
                format!("Validated {} PlanV2 step(s).", plan.steps.len()),
            ),
        );
        for step in &plan.steps {
            emit_progress(
                progress,
                PipelineProgressEvent::step(
                    "pending",
                    &step.id,
                    op_kind_name(&step.op),
                    None,
                    None,
                ),
            );
        }
        let raw_plan_block = raw_plan_json
            .as_deref()
            .map(|json| format!("Raw Planner JSON:\n```json\n{json}\n```"))
            .unwrap_or_else(|| {
                "Raw Planner JSON:\n```text\nplanner response did not parse as valid PlanV2 JSON\n```"
                    .to_string()
            });
        let raw_repair_block = raw_repair_plan_json
            .as_deref()
            .map(|json| format!("\n\nRaw Repair JSON:\n```json\n{json}\n```"))
            .unwrap_or_default();
        let debug_plan_text = format!(
            "{}\n\n{}\n\n{}{}\n\n{}",
            entity_mention_hints,
            entity_resolution_hints,
            raw_plan_block,
            raw_repair_block,
            render_multistep_plan(&plan)
        );
        if !execute {
            return Ok(debug_plan_text);
        }
        let execution_started = Instant::now();
        emit_progress(
            progress,
            PipelineProgressEvent::stage("execution", "running", "Executing the validated plan."),
        );
        let (deterministic, effective_queries, execution_evidence, execution_groundings) =
            execute_multistep_plan_with_progress(
                state,
                &schema_registry,
                model_name,
                user_message,
                &plan,
                progress,
            )
            .await?;
        let execution_ms = execution_started.elapsed().as_millis();
        emit_progress(
            progress,
            PipelineProgressEvent::stage(
                "execution",
                "completed",
                format!("Execution completed in {execution_ms}ms."),
            ),
        );
        let combined_query_text = executed_query_text(&effective_queries);
        let planned_query_text = plan
            .steps
            .iter()
            .filter_map(|step| step.query.as_deref())
            .collect::<Vec<_>>()
            .join("\n");
        let mut scope_used = scope_used_summary(&planned_query_text, &combined_query_text);
        let post_fetch_constraints = collect_post_fetch_scope_constraints(&plan);
        if !post_fetch_constraints.is_empty()
            && let Some(obj) = scope_used.as_object_mut()
        {
            obj.insert(
                "post_fetch_constraints".to_string(),
                serde_json::Value::Array(
                    post_fetch_constraints
                        .iter()
                        .map(scope_constraint_to_json)
                        .collect::<Vec<_>>(),
                ),
            );
        }
        let mut missing_scope = scope_used
            .get("missing_constraints")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|v| scope_constraint_from_json(&v))
            .collect::<Vec<_>>();
        if !missing_scope.is_empty() && !execution_evidence.field_values.is_empty() {
            let planned_constraints =
                planned_constraints_from_query(&schema_registry, &planned_query_text);
            let (remaining, matched) = reconcile_missing_with_evidence(
                &missing_scope,
                &planned_constraints,
                &execution_evidence,
            );
            if let Some(obj) = scope_used.as_object_mut() {
                obj.insert(
                    "evidence_matched_constraints".to_string(),
                    serde_json::Value::Array(
                        matched
                            .iter()
                            .map(scope_constraint_to_json)
                            .collect::<Vec<_>>(),
                    ),
                );
                obj.insert(
                    "evidence_literal_sample".to_string(),
                    serde_json::json!(
                        execution_evidence
                            .literals
                            .iter()
                            .take(20)
                            .cloned()
                            .collect::<Vec<_>>()
                    ),
                );
                obj.insert(
                    "evidence_row_count".to_string(),
                    serde_json::json!(execution_evidence.row_count),
                );
                obj.insert(
                    "evidence_time_values_sample".to_string(),
                    serde_json::json!(
                        execution_evidence
                            .time_values
                            .iter()
                            .take(10)
                            .cloned()
                            .collect::<Vec<_>>()
                    ),
                );
            }
            missing_scope = remaining;
        }
        if !missing_scope.is_empty() {
            let guard_items = missing_scope
                .iter()
                .map(format_scope_constraint)
                .collect::<Vec<_>>();
            let guarded =
                backend_placeholder_scope_issue_message(&missing_scope, &plan, &effective_queries)
                    .unwrap_or_else(|| scope_guard_message(&guard_items));
            if debug_output {
                return Ok(format!(
                    "{}\n\n{}\n\nscope_used:\n```json\n{}\n```\n\nFinal Answer:\n{}",
                    debug_plan_text,
                    render_effective_queries(&effective_queries),
                    serde_json::to_string_pretty(&scope_used).unwrap_or_default(),
                    guarded
                ));
            }
            return Ok(guarded);
        }
        if let Some(guarded) = location_label_capability_gap(
            user_message,
            &scope_used,
            &execution_evidence,
            state.sls.as_ref(),
        ) {
            if debug_output {
                return Ok(format!(
                    "{}\n\n{}\n\nscope_used:\n```json\n{}\n```\n\nFinal Answer:\n{}",
                    debug_plan_text,
                    render_effective_queries(&effective_queries),
                    serde_json::to_string_pretty(&scope_used).unwrap_or_default(),
                    guarded
                ));
            }
            return Ok(guarded);
        }
        if !plan_has_explicit_scope_step(&plan)
            && user_has_entity_scope_request(&schema_registry, user_message)
        {
            let guarded = scope_guard_message(&[
                "user entity constraints were not carried into fetch/filter steps".to_string(),
            ]);
            if debug_output {
                return Ok(format!(
                    "{}\n\n{}\n\nscope_used:\n```json\n{}\n```\n\nFinal Answer:\n{}",
                    debug_plan_text,
                    render_effective_queries(&effective_queries),
                    serde_json::to_string_pretty(&scope_used).unwrap_or_default(),
                    guarded
                ));
            }
            return Ok(guarded);
        }
        let entity_resolutions =
            merge_execution_groundings_into_resolutions(&entity_resolutions, &execution_groundings);
        let evidence = serde_json::json!({
            "mode": "multi_step",
            "plan": plan,
            "effective_queries": effective_queries,
            "computed_result": deterministic.text,
            "deterministic_kind": format!("{:?}", deterministic.kind),
            "scope_used": scope_used,
            "evidence_row_count": execution_evidence.row_count,
            "evidence_sample_rows": execution_evidence.sample_rows,
            "evidence_literal_sample": execution_evidence.literals.iter().take(20).cloned().collect::<Vec<_>>(),
            "evidence_time_values_sample": execution_evidence.time_values.iter().take(10).cloned().collect::<Vec<_>>()
        });
        let answer_started = Instant::now();
        emit_progress(
            progress,
            PipelineProgressEvent::stage(
                "answer_synthesis",
                "running",
                "Preparing grounded answer.",
            ),
        );
        let use_deterministic_answer = deterministic_answer_is_final(&deterministic.kind);
        let answer = if use_deterministic_answer {
            deterministic.text.clone()
        } else {
            let synthesized = synthesize_answer_with_llm(
                state,
                model_name,
                user_message,
                &evidence,
                &deterministic.text,
            )
            .await;
            if let Some(usage) = synthesized.token_usage {
                provider_token_usage += usage;
            }
            synthesized.text
        };
        let answer_synthesis_ms = if use_deterministic_answer {
            None
        } else {
            Some(answer_started.elapsed().as_millis())
        };
        emit_progress(
            progress,
            PipelineProgressEvent::stage(
                "answer_synthesis",
                "completed",
                "Grounded answer is ready.",
            ),
        );
        let metrics = PipelineMetrics {
            total_pipeline_ms: pipeline_started.elapsed().as_millis(),
            provider_prompt_cache: provider_profile.clone(),
            planner_ms,
            planner_cache_hit,
            planner_repair_cache_hit,
            planner_prompt_chars: ir_prompt.len(),
            planner_prompt_tokens_est: estimate_tokens(&ir_prompt),
            planner_stable_prefix_chars: planner_prompt_cache_shape.stable_prefix_chars,
            planner_stable_prefix_tokens_est: planner_prompt_cache_shape.stable_prefix_tokens_est,
            planner_variable_suffix_chars: planner_prompt_cache_shape.variable_suffix_chars,
            planner_variable_suffix_tokens_est: planner_prompt_cache_shape
                .variable_suffix_tokens_est,
            planner_response_chars: ir_response.len(),
            planner_response_tokens_est: estimate_tokens(&ir_response),
            planner_repair_ms,
            planner_repair_prompt_chars,
            planner_repair_prompt_tokens_est,
            planner_repair_response_chars: repair_response_text.as_ref().map(String::len),
            planner_repair_response_tokens_est: repair_response_text
                .as_deref()
                .map(estimate_tokens),
            execution_ms: Some(execution_ms),
            answer_synthesis_ms,
            answer_chars: answer.len(),
            answer_tokens_est: estimate_tokens(&answer),
            provider_token_usage_available: provider_token_usage.is_available(),
            provider_token_usage: if provider_token_usage.is_available() {
                Some(provider_token_usage)
            } else {
                None
            },
        };
        if debug_output {
            let provenance = build_provenance_payload(
                &plan,
                &effective_queries,
                &scope_used,
                &execution_evidence,
                &deterministic,
                &answer,
                &entity_mentions,
                &entity_resolutions,
                raw_plan_json.as_deref(),
                raw_repair_plan_json.as_deref(),
                &metrics,
                &schema_retrieval,
            );
            return Ok(format!(
                "{}\n\n{}\n\nscope_used:\n```json\n{}\n```\n\nDeterministic (pre-LLM):\n{}\n\nFinal Answer:\n{}\n\nProvenance:\n```json\n{}\n```",
                debug_plan_text,
                render_effective_queries(&effective_queries),
                serde_json::to_string_pretty(&scope_used).unwrap_or_default(),
                deterministic.text,
                answer,
                serde_json::to_string_pretty(&provenance).unwrap_or_default()
            ));
        }
        return Ok(answer);
    }

    if let Some(answer) = introspection_answer(&schema_registry, user_message) {
        return Ok(answer);
    }
    if debug_output {
        return Err(PipelineError::planning(format!(
            "expected valid PlanV2 output after repair\n\ninitial_model_output:\n{}\n\ninitial_plan_error:\n{}\n\nrepair_output:\n{}\n\nrepair_plan_error:\n{}",
            ir_response,
            last_plan_error.unwrap_or_else(|| "none".to_string()),
            repair_response_text.unwrap_or_else(|| "none".to_string()),
            repair_plan_error.unwrap_or_else(|| "none".to_string())
        )));
    }
    Err(PipelineError::planning(
        "expected valid PlanV2 output after repair",
    ))
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ScopeConstraintKind {
    Time,
    Entity,
    Other,
}

#[derive(Clone, Debug)]
struct PlannedConstraint {
    kind: ScopeConstraintKind,
    root: String,
    field: String,
    op: Option<String>,
    values: Vec<String>,
}

fn estimate_tokens(text: &str) -> usize {
    let chars = text.chars().count();
    if chars == 0 { 0 } else { chars.div_ceil(4) }
}

#[derive(Clone, Debug)]
struct PlannerPromptCacheShape {
    stable_prefix_chars: usize,
    stable_prefix_tokens_est: usize,
    variable_suffix_chars: usize,
    variable_suffix_tokens_est: usize,
}

#[allow(clippy::too_many_arguments)]
fn estimate_planner_prompt_cache_shape(
    today_utc: &str,
    root_fields: &str,
    preferred_root_fields: &str,
    planner_examples: &str,
    entity_mention_hints: &str,
    entity_resolution_hints: &str,
    policy_hints: &str,
    join_hints: &str,
    metric_hints: &str,
    field_hints: &str,
    schema_snippet: &str,
    user_message: &str,
) -> PlannerPromptCacheShape {
    let stable_prefix = [
        root_fields,
        preferred_root_fields,
        planner_examples,
        policy_hints,
        join_hints,
        metric_hints,
        field_hints,
        schema_snippet,
    ]
    .join("\n\n");
    let variable_suffix = [
        today_utc,
        entity_mention_hints,
        entity_resolution_hints,
        user_message,
    ]
    .join("\n\n");

    PlannerPromptCacheShape {
        stable_prefix_chars: stable_prefix.len(),
        stable_prefix_tokens_est: estimate_tokens(&stable_prefix),
        variable_suffix_chars: variable_suffix.len(),
        variable_suffix_tokens_est: estimate_tokens(&variable_suffix),
    }
}

fn schema_retrieval_summary(
    slice: &SchemaRetrievalSlice,
    budget: crate::schema_registry::PlannerRetrievalBudget,
) -> serde_json::Value {
    serde_json::json!({
        "mode": "retrieved",
        "budget": {
            "root_limit": budget.root_limit,
            "field_limit": budget.field_limit,
            "entity_resolution_limit": budget.entity_resolution_limit
        },
        "intent": slice.intent,
        "confidence": slice.profile.confidence.as_str(),
        "top_score": slice.profile.top_score,
        "runner_up_score": slice.profile.runner_up_score,
        "competitive_root_count": slice.profile.competitive_root_count,
        "roots": slice.roots
    })
}

fn anchored_schema_retrieval_summary(
    roots: &[String],
    budget: crate::schema_registry::PlannerRetrievalBudget,
) -> serde_json::Value {
    serde_json::json!({
        "mode": "entity_anchored",
        "budget": {
            "root_limit": budget.root_limit,
            "field_limit": budget.field_limit,
            "entity_resolution_limit": budget.entity_resolution_limit
        },
        "anchored_roots": roots
    })
}

#[derive(Clone, Debug, serde::Serialize)]
struct PipelineMetrics {
    total_pipeline_ms: u128,
    provider_prompt_cache: ProviderPromptCacheProfile,
    planner_ms: u128,
    planner_cache_hit: bool,
    planner_repair_cache_hit: Option<bool>,
    planner_prompt_chars: usize,
    planner_prompt_tokens_est: usize,
    planner_stable_prefix_chars: usize,
    planner_stable_prefix_tokens_est: usize,
    planner_variable_suffix_chars: usize,
    planner_variable_suffix_tokens_est: usize,
    planner_response_chars: usize,
    planner_response_tokens_est: usize,
    planner_repair_ms: Option<u128>,
    planner_repair_prompt_chars: Option<usize>,
    planner_repair_prompt_tokens_est: Option<usize>,
    planner_repair_response_chars: Option<usize>,
    planner_repair_response_tokens_est: Option<usize>,
    execution_ms: Option<u128>,
    answer_synthesis_ms: Option<u128>,
    answer_chars: usize,
    answer_tokens_est: usize,
    provider_token_usage_available: bool,
    provider_token_usage: Option<ProviderTokenUsage>,
}

#[derive(Clone, Debug)]
struct ScopeConstraint {
    root: String,
    field: String,
    op: Option<String>,
    values: Vec<String>,
}

fn normalize_scope_operator(op: &str) -> String {
    match op.to_ascii_lowercase().as_str() {
        "gte" => "ge".to_string(),
        "lte" => "le".to_string(),
        other => other.to_string(),
    }
}

fn normalize_scope_value(value: &str) -> String {
    value
        .chars()
        .filter(|ch| !ch.is_ascii_whitespace())
        .map(|ch| ch.to_ascii_lowercase())
        .collect()
}

fn scope_values_match(left: &str, right: &str) -> bool {
    if left.eq_ignore_ascii_case(right) {
        return true;
    }
    let left_norm = normalize_scope_value(left);
    let right_norm = normalize_scope_value(right);
    !left_norm.is_empty() && left_norm == right_norm
}

fn scope_operator_preserves_constraint(planned: &str, executed: &str) -> bool {
    let planned = normalize_scope_operator(planned);
    let executed = normalize_scope_operator(executed);
    planned.eq_ignore_ascii_case(&executed) || (planned == "eq" && executed == "in")
}

fn scope_constraint_from_json(value: &serde_json::Value) -> Option<ScopeConstraint> {
    let obj = value.as_object()?;
    let root = obj.get("root")?.as_str()?.to_string();
    let field = obj.get("field")?.as_str()?.to_string();
    let op = obj
        .get("op")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let values = obj
        .get("values")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|item| match item {
                    serde_json::Value::String(s) => {
                        let trimmed = s.trim();
                        if trimmed.is_empty() {
                            None
                        } else {
                            Some(trimmed.to_string())
                        }
                    }
                    serde_json::Value::Number(n) => Some(n.to_string()),
                    serde_json::Value::Bool(b) => Some(b.to_string()),
                    _ => None,
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Some(ScopeConstraint {
        root,
        field,
        op,
        values,
    })
}

fn scope_constraint_to_json(constraint: &ScopeConstraint) -> serde_json::Value {
    serde_json::json!({
        "root": constraint.root,
        "field": constraint.field,
        "op": constraint.op.as_deref().unwrap_or("eq"),
        "values": constraint.values
    })
}

fn format_scope_constraint(constraint: &ScopeConstraint) -> String {
    let op = constraint.op.as_deref().unwrap_or("eq");
    if constraint.values.len() == 1 {
        format!(
            "{}.{} {} {}",
            constraint.root, constraint.field, op, constraint.values[0]
        )
    } else {
        format!(
            "{}.{} {} [{}]",
            constraint.root,
            constraint.field,
            op,
            constraint.values.join(", ")
        )
    }
}

fn placeholder_source(value: &str) -> Option<(&str, &str)> {
    let inner = value.trim().strip_prefix("${")?.strip_suffix("}")?;
    let (step_id, field_path) = inner.split_once('.')?;
    if step_id.trim().is_empty() || field_path.trim().is_empty() {
        return None;
    }
    Some((step_id.trim(), field_path.trim()))
}

fn step_root_from_plan(
    step_id: &str,
    steps_by_id: &HashMap<&str, &crate::planner::ExecutableStep>,
) -> Option<String> {
    let step = steps_by_id.get(step_id)?;
    match &step.op {
        crate::planner::PlanV2Op::Fetch { root_field, .. } => Some(root_field.clone()),
        crate::planner::PlanV2Op::Aggregate { source, .. }
        | crate::planner::PlanV2Op::FilterRows { source, .. }
        | crate::planner::PlanV2Op::Rank { source, .. }
        | crate::planner::PlanV2Op::ThresholdCheck { source, .. }
        | crate::planner::PlanV2Op::TrendSummary { source, .. } => {
            step_root_from_plan(source, steps_by_id)
        }
        crate::planner::PlanV2Op::Compare { left, .. } => step_root_from_plan(left, steps_by_id),
        crate::planner::PlanV2Op::DistanceHaversine { .. }
        | crate::planner::PlanV2Op::JoinOnTime { .. } => None,
    }
}

fn repair_trace_for_step<'a>(
    effective_queries: &'a [crate::planner::ExecutedArtifact],
    step_id: &str,
) -> Option<&'a str> {
    let marker = format!("[QUERY_REPAIR_TRACE] {step_id}:");
    for artifact in effective_queries {
        if artifact.kind != ExecutedArtifactKind::DebugLog {
            continue;
        }
        let Some(start) = artifact.body.find(&marker) else {
            continue;
        };
        let tail = &artifact.body[start + marker.len()..];
        let end = tail.find("[QUERY_REPAIR_TRACE] ").unwrap_or(tail.len());
        return Some(tail[..end].trim());
    }
    None
}

fn backend_placeholder_scope_issue_message(
    missing: &[ScopeConstraint],
    plan: &crate::planner::MultiStepPlan,
    effective_queries: &[crate::planner::ExecutedArtifact],
) -> Option<String> {
    let steps_by_id = plan
        .steps
        .iter()
        .map(|step| (step.id.as_str(), step))
        .collect::<HashMap<_, _>>();
    let mut backend_mismatches = Vec::new();

    for constraint in missing {
        for value in &constraint.values {
            let Some((step_id, field_path)) = placeholder_source(value) else {
                continue;
            };
            let Some(source_field) = field_path
                .split('.')
                .next()
                .map(str::trim)
                .filter(|field| !field.is_empty())
            else {
                continue;
            };
            let Some(trace) = repair_trace_for_step(effective_queries, step_id) else {
                continue;
            };
            if !trace.contains(&format!("Unknown field \"{source_field}\"")) {
                continue;
            }
            let source_root = step_root_from_plan(step_id, &steps_by_id)
                .unwrap_or_else(|| format!("step {step_id}"));
            let detail = format!(
                "{}.{} (needed for {})",
                source_root,
                source_field,
                format_scope_constraint(constraint)
            );
            if !backend_mismatches
                .iter()
                .any(|existing| existing == &detail)
            {
                backend_mismatches.push(detail);
            }
        }
    }

    if backend_mismatches.is_empty() {
        return None;
    }

    Some(format!(
        "I found the parent entity, but I still can’t answer this scoped request because the backend rejected the source field(s) needed to preserve scope: {}. This looks like a backend/schema mismatch rather than a true empty result.",
        backend_mismatches.join(", ")
    ))
}

fn filter_rows_value_strings(value: &serde_json::Value) -> Vec<String> {
    match value {
        serde_json::Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                Vec::new()
            } else {
                vec![trimmed.to_string()]
            }
        }
        serde_json::Value::Array(items) => items
            .iter()
            .filter_map(|item| match item {
                serde_json::Value::String(text) => {
                    let trimmed = text.trim();
                    (!trimmed.is_empty()).then_some(trimmed.to_string())
                }
                serde_json::Value::Number(n) => Some(n.to_string()),
                serde_json::Value::Bool(b) => Some(b.to_string()),
                _ => None,
            })
            .collect(),
        serde_json::Value::Number(n) => vec![n.to_string()],
        serde_json::Value::Bool(b) => vec![b.to_string()],
        _ => Vec::new(),
    }
}

fn collect_post_fetch_scope_constraints(
    plan: &crate::planner::MultiStepPlan,
) -> Vec<ScopeConstraint> {
    let steps_by_id = plan
        .steps
        .iter()
        .map(|step| (step.id.as_str(), step))
        .collect::<HashMap<_, _>>();
    let mut out = Vec::new();
    for step in &plan.steps {
        let crate::planner::PlanV2Op::FilterRows {
            source,
            field,
            operator,
            value,
        } = &step.op
        else {
            continue;
        };
        let values = filter_rows_value_strings(value);
        if values.is_empty() {
            continue;
        }
        let Some(root) = step_root_from_plan(source, &steps_by_id) else {
            continue;
        };
        out.push(ScopeConstraint {
            root,
            field: field.clone(),
            op: Some(operator.clone()),
            values,
        });
    }
    out
}

fn planned_constraints_from_query(
    schema_registry: &crate::schema_registry::SchemaRegistry,
    query_text: &str,
) -> Vec<PlannedConstraint> {
    let Ok(doc) = parse_query::<String>(query_text) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for def in doc.definitions {
        let selection_set = match def {
            QueryDefinition::Operation(OperationDefinition::Query(q)) => q.selection_set,
            QueryDefinition::Operation(OperationDefinition::SelectionSet(set)) => set,
            _ => continue,
        };
        for selection in selection_set.items {
            let Selection::Field(field) = selection else {
                continue;
            };
            let root = field.name.clone();
            let filter_value = field
                .arguments
                .iter()
                .find(|(name, _)| name == "filter")
                .map(|(_, value)| value);
            if let Some(filter) = filter_value {
                collect_constraints_for_filter(
                    schema_registry,
                    &root,
                    filter,
                    String::new(),
                    &mut out,
                );
            }
        }
    }
    out
}

fn collect_constraints_for_filter(
    schema_registry: &crate::schema_registry::SchemaRegistry,
    root: &str,
    value: &QueryValue<'_, String>,
    path: String,
    out: &mut Vec<PlannedConstraint>,
) {
    match value {
        QueryValue::Object(map) => {
            for (key, value) in map {
                let key_lower = key.to_ascii_lowercase();
                if matches!(key_lower.as_str(), "and" | "or" | "not") {
                    collect_constraints_for_filter(schema_registry, root, value, path.clone(), out);
                    continue;
                }
                if is_operator_key(&key_lower) && path.is_empty() {
                    continue;
                }
                if is_operator_key(&key_lower) && !path.is_empty() {
                    let mut values = Vec::new();
                    collect_literal_values(value, &mut values);
                    if !values.is_empty() {
                        add_constraint(schema_registry, root, &path, Some(key_lower), values, out);
                    }
                    continue;
                }
                let new_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{}.{}", path, key)
                };
                match value {
                    QueryValue::Object(_) | QueryValue::List(_) => {
                        collect_constraints_for_filter(schema_registry, root, value, new_path, out);
                    }
                    _ => {
                        let mut values = Vec::new();
                        collect_literal_values(value, &mut values);
                        if !values.is_empty() {
                            add_constraint(
                                schema_registry,
                                root,
                                &new_path,
                                Some("eq".to_string()),
                                values,
                                out,
                            );
                        }
                    }
                }
            }
        }
        QueryValue::List(items) => {
            for item in items {
                collect_constraints_for_filter(schema_registry, root, item, path.clone(), out);
            }
        }
        _ => {
            if path.is_empty() {
                return;
            }
            let mut values = Vec::new();
            collect_literal_values(value, &mut values);
            if !values.is_empty() {
                add_constraint(
                    schema_registry,
                    root,
                    &path,
                    Some("eq".to_string()),
                    values,
                    out,
                );
            }
        }
    }
}

fn collect_literal_values(value: &QueryValue<'_, String>, out: &mut Vec<String>) {
    match value {
        QueryValue::String(s) | QueryValue::Enum(s) => {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                out.push(trimmed.to_string());
            }
        }
        QueryValue::Int(n) => {
            if let Some(v) = n.as_i64() {
                out.push(v.to_string());
            }
        }
        QueryValue::Float(n) => {
            out.push(n.to_string());
        }
        QueryValue::List(items) => {
            for item in items {
                collect_literal_values(item, out);
            }
        }
        QueryValue::Object(map) => {
            for value in map.values() {
                collect_literal_values(value, out);
            }
        }
        QueryValue::Boolean(b) => {
            out.push(b.to_string());
        }
        _ => {}
    }
}

fn is_operator_key(key: &str) -> bool {
    matches!(
        key,
        "eq" | "ne"
            | "contains"
            | "gt"
            | "gte"
            | "ge"
            | "lt"
            | "lte"
            | "le"
            | "in"
            | "between"
            | "like"
    )
}

fn add_constraint(
    schema_registry: &crate::schema_registry::SchemaRegistry,
    root: &str,
    path: &str,
    op: Option<String>,
    values: Vec<String>,
    out: &mut Vec<PlannedConstraint>,
) {
    let top = path.split('.').next().unwrap_or(path).to_ascii_lowercase();
    if matches!(top.as_str(), "and" | "or" | "not") {
        return;
    }
    let time_fields = schema_registry
        .root_time_filter_fields(root)
        .into_iter()
        .map(|v| v.to_ascii_lowercase())
        .collect::<BTreeSet<_>>();
    let id_fields = schema_registry
        .root_identifier_filter_fields(root)
        .into_iter()
        .map(|v| v.to_ascii_lowercase())
        .collect::<BTreeSet<_>>();
    let kind = if time_fields.contains(&top) {
        ScopeConstraintKind::Time
    } else if id_fields.contains(&top) {
        ScopeConstraintKind::Entity
    } else {
        ScopeConstraintKind::Other
    };
    if let Some(existing) = out.iter_mut().find(|c| {
        c.root.eq_ignore_ascii_case(root)
            && c.field.eq_ignore_ascii_case(path)
            && c.op.as_deref().unwrap_or("eq") == op.as_deref().unwrap_or("eq")
            && c.kind == kind
    }) {
        for value in values {
            if !existing.values.iter().any(|v| v == &value) {
                existing.values.push(value);
            }
        }
        return;
    }
    out.push(PlannedConstraint {
        kind,
        root: root.to_string(),
        field: path.to_string(),
        op,
        values,
    });
}

fn reconcile_missing_with_evidence(
    missing: &[ScopeConstraint],
    planned_constraints: &[PlannedConstraint],
    evidence: &ExecutionEvidence,
) -> (Vec<ScopeConstraint>, Vec<ScopeConstraint>) {
    if missing.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let mut remaining = Vec::new();
    let mut matched = Vec::new();
    for item in missing {
        if item.values.is_empty() {
            remaining.push(item.clone());
            continue;
        }
        let planned = planned_constraints
            .iter()
            .filter(|c| {
                c.root.eq_ignore_ascii_case(&item.root)
                    && c.field.eq_ignore_ascii_case(&item.field)
                    && scope_operator_preserves_constraint(
                        item.op.as_deref().unwrap_or("eq"),
                        c.op.as_deref().unwrap_or("eq"),
                    )
                    && c.values
                        .iter()
                        .any(|v| item.values.iter().any(|mv| scope_values_match(mv, v)))
            })
            .collect::<Vec<_>>();
        let matched_evidence = planned
            .iter()
            .any(|constraint| constraint_matches_evidence(constraint, evidence));
        if matched_evidence {
            matched.push(item.clone());
        } else {
            remaining.push(item.clone());
        }
    }
    (remaining, matched)
}

fn constraint_matches_evidence(
    constraint: &PlannedConstraint,
    evidence: &ExecutionEvidence,
) -> bool {
    let Some(evidence_values) = evidence_values_for_field(evidence, constraint.field.as_str())
    else {
        return false;
    };
    if evidence_values.is_empty() {
        return false;
    }

    let op = constraint
        .op
        .as_deref()
        .unwrap_or("eq")
        .to_ascii_lowercase();

    match constraint.kind {
        ScopeConstraintKind::Time => {
            return matches_time_constraint(&op, constraint, evidence_values);
        }
        ScopeConstraintKind::Entity | ScopeConstraintKind::Other => {}
    }

    if matches_numeric_operator(op.as_str()) {
        if let Some(matched) = matches_numeric_constraint(&op, &constraint.values, evidence_values)
        {
            return matched;
        }
        return false;
    }

    if matches_numeric_operator_or_equality(op.as_str())
        && let Some(matched) = matches_numeric_constraint(&op, &constraint.values, evidence_values)
    {
        return matched;
    }

    matches_string_constraint(&op, &constraint.values, evidence_values)
}

fn evidence_values_for_field<'a>(
    evidence: &'a ExecutionEvidence,
    field: &str,
) -> Option<&'a Vec<String>> {
    if let Some(values) = evidence.field_values.get(field) {
        return Some(values);
    }
    let field_lower = field.to_ascii_lowercase();
    evidence
        .field_values
        .iter()
        .find(|(key, _)| key.to_ascii_lowercase() == field_lower)
        .map(|(_, values)| values)
}

fn matches_time_constraint(
    op: &str,
    constraint: &PlannedConstraint,
    evidence_values: &[String],
) -> bool {
    let mut evidence_times = evidence_values
        .iter()
        .filter_map(|value| parse_time_millis_from_str(value))
        .collect::<Vec<_>>();
    if evidence_times.is_empty() {
        return false;
    }
    let mut values = constraint
        .values
        .iter()
        .filter_map(|v| parse_time_millis_from_str(v))
        .collect::<Vec<_>>();
    if values.is_empty() {
        return false;
    }
    evidence_times.sort();
    values.sort();
    match op {
        "between" => {
            if values.len() < 2 {
                return false;
            }
            let min = values[0];
            let max = values[values.len() - 1];
            evidence_times.iter().any(|t| *t >= min && *t <= max)
        }
        "in" => evidence_times.iter().any(|t| values.contains(t)),
        "gt" => evidence_times.iter().any(|t| *t > values[0]),
        "ge" | "gte" => evidence_times.iter().any(|t| *t >= values[0]),
        "lt" => evidence_times.iter().any(|t| *t < values[0]),
        "le" | "lte" => evidence_times.iter().any(|t| *t <= values[0]),
        "ne" => evidence_times.iter().all(|t| !values.contains(t)),
        _ => evidence_times.iter().any(|t| values.contains(t)),
    }
}

fn matches_numeric_operator(op: &str) -> bool {
    matches!(op, "gt" | "ge" | "gte" | "lt" | "le" | "lte" | "between")
}

fn matches_numeric_operator_or_equality(op: &str) -> bool {
    matches!(
        op,
        "gt" | "ge" | "gte" | "lt" | "le" | "lte" | "between" | "eq" | "in" | "ne"
    )
}

fn matches_numeric_constraint(
    op: &str,
    constraint_values: &[String],
    evidence_values: &[String],
) -> Option<bool> {
    let constraint_nums = constraint_values
        .iter()
        .filter_map(|v| parse_numeric_value(v))
        .collect::<Vec<_>>();
    if constraint_nums.len() != constraint_values.len() || constraint_nums.is_empty() {
        return None;
    }
    let evidence_nums = evidence_values
        .iter()
        .filter_map(|v| parse_numeric_value(v))
        .collect::<Vec<_>>();
    if evidence_nums.is_empty() {
        return None;
    }
    let mut values = constraint_nums;
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    match op {
        "between" => {
            if values.len() < 2 {
                return Some(false);
            }
            let min = values[0];
            let max = values[values.len() - 1];
            Some(evidence_nums.iter().any(|v| *v >= min && *v <= max))
        }
        "in" => Some(evidence_nums.iter().any(|v| values.contains(v))),
        "gt" => Some(evidence_nums.iter().any(|v| *v > values[0])),
        "ge" | "gte" => Some(evidence_nums.iter().any(|v| *v >= values[0])),
        "lt" => Some(evidence_nums.iter().any(|v| *v < values[0])),
        "le" | "lte" => Some(evidence_nums.iter().any(|v| *v <= values[0])),
        "ne" => Some(evidence_nums.iter().all(|v| !values.contains(v))),
        _ => Some(evidence_nums.iter().any(|v| values.contains(v))),
    }
}

#[allow(clippy::too_many_arguments)]
fn build_provenance_payload(
    plan: &crate::planner::MultiStepPlan,
    effective_queries: &[crate::planner::ExecutedArtifact],
    scope_used: &serde_json::Value,
    evidence: &ExecutionEvidence,
    deterministic: &DeterministicAnswer,
    answer: &str,
    entity_mentions: &[String],
    entity_resolutions: &[crate::entity_linker::EntityResolution],
    raw_plan_json: Option<&str>,
    raw_repair_plan_json: Option<&str>,
    metrics: &PipelineMetrics,
    schema_retrieval: &serde_json::Value,
) -> serde_json::Value {
    let executed_queries = effective_queries
        .iter()
        .filter(|q| q.kind == ExecutedArtifactKind::Query)
        .map(|q| serde_json::json!({ "title": q.title, "query": q.body }))
        .collect::<Vec<_>>();

    let mut field_values = serde_json::Map::new();
    for (field, values) in &evidence.field_values {
        let capped = values.iter().take(10).cloned().collect::<Vec<_>>();
        field_values.insert(field.clone(), serde_json::json!(capped));
    }

    let traceability = build_claim_traceability(answer, evidence);
    let grounding_confidence = build_grounding_confidence_signal(entity_resolutions);
    let uncertainty = build_uncertainty_signal(
        effective_queries,
        schema_retrieval,
        &grounding_confidence,
        raw_repair_plan_json,
        deterministic,
        answer,
        scope_used,
    );

    serde_json::json!({
        "answer": answer,
        "deterministic_answer": {
            "text": deterministic.text,
            "kind": format!("{:?}", deterministic.kind)
        },
        "entity_mentions": entity_mentions,
        "entity_resolution": entity_resolutions,
        "grounding_confidence": grounding_confidence,
        "uncertainty": uncertainty,
        "executed_queries": executed_queries,
        "plan_steps": plan.steps,
        "planner_raw_plan_json": raw_plan_json,
        "planner_repair_raw_plan_json": raw_repair_plan_json,
        "schema_retrieval": schema_retrieval,
        "metrics": metrics,
        "scope_used": scope_used,
        "evidence": {
            "row_count": evidence.row_count,
            "sample_rows": evidence.sample_rows,
            "literal_sample": evidence.literals.iter().take(20).cloned().collect::<Vec<_>>(),
            "time_values_sample": evidence.time_values.iter().take(10).cloned().collect::<Vec<_>>(),
            "field_values": field_values
        },
        "claim_traceability": traceability
    })
}

fn push_uncertainty_signal(
    signals: &mut Vec<serde_json::Value>,
    kind: &str,
    severity: &str,
    message: &str,
) {
    if signals.iter().any(|signal| {
        signal.get("kind").and_then(|value| value.as_str()) == Some(kind)
            && signal.get("message").and_then(|value| value.as_str()) == Some(message)
    }) {
        return;
    }
    signals.push(serde_json::json!({
        "kind": kind,
        "severity": severity,
        "message": message,
    }));
}

fn build_uncertainty_signal(
    effective_queries: &[crate::planner::ExecutedArtifact],
    schema_retrieval: &serde_json::Value,
    grounding_confidence: &serde_json::Value,
    raw_repair_plan_json: Option<&str>,
    deterministic: &DeterministicAnswer,
    answer: &str,
    scope_used: &serde_json::Value,
) -> serde_json::Value {
    let mut signals = Vec::new();

    if let Some(confidence) = schema_retrieval
        .get("confidence")
        .and_then(|value| value.as_str())
        && matches!(confidence, "low" | "medium")
    {
        push_uncertainty_signal(
            &mut signals,
            "schema_retrieval_confidence",
            if confidence == "low" {
                "high"
            } else {
                "medium"
            },
            &format!("Schema retrieval confidence was {confidence}."),
        );
    }
    if let Some(competitive_roots) = schema_retrieval
        .get("competitive_root_count")
        .and_then(|value| value.as_u64())
        && competitive_roots > 1
    {
        push_uncertainty_signal(
            &mut signals,
            "competitive_schema_roots",
            "medium",
            &format!("{competitive_roots} schema roots were competitive for this request."),
        );
    }

    if raw_repair_plan_json.is_some() {
        push_uncertainty_signal(
            &mut signals,
            "planner_repair_used",
            "medium",
            "The initial PlanV2 needed a repair pass before execution.",
        );
    }

    let debug_text = effective_queries
        .iter()
        .filter(|artifact| artifact.kind == ExecutedArtifactKind::DebugLog)
        .map(|artifact| artifact.body.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let debug_lower = debug_text.to_ascii_lowercase();
    if debug_lower.contains("empty-rows retry")
        || debug_lower.contains("next_query(deterministic)")
        || debug_lower.contains("identifier fallback")
    {
        push_uncertainty_signal(
            &mut signals,
            "query_repair_or_fallback_used",
            "medium",
            "Execution needed query repair or a deterministic fallback to obtain rows.",
        );
    }

    let answer_context = format!("{}\n{}", deterministic.text, answer).to_ascii_lowercase();
    if debug_lower.contains("backend returned no child rows")
        || debug_lower.contains("returned no child rows")
        || answer_context.contains("backend returned no child rows")
        || answer_context.contains("backend/schema relation issue")
    {
        push_uncertainty_signal(
            &mut signals,
            "backend_relation_gap",
            "high",
            "A schema-supported relation returned no child rows from the backend.",
        );
    }

    if scope_used
        .get("missing_constraints")
        .and_then(|value| value.as_array())
        .is_some_and(|items| !items.is_empty())
    {
        push_uncertainty_signal(
            &mut signals,
            "scope_not_fully_applied",
            "high",
            "One or more planned scope constraints were not present in the executed query.",
        );
    }

    if grounding_confidence
        .get("clarification_recommended")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
    {
        push_uncertainty_signal(
            &mut signals,
            "entity_confirmation_recommended",
            "high",
            "Entity grounding needs user confirmation before safe execution.",
        );
    }

    let overall = if signals
        .iter()
        .any(|signal| signal.get("severity").and_then(|value| value.as_str()) == Some("high"))
    {
        "high"
    } else if signals
        .iter()
        .any(|signal| signal.get("severity").and_then(|value| value.as_str()) == Some("medium"))
    {
        "medium"
    } else if signals
        .iter()
        .any(|signal| signal.get("severity").and_then(|value| value.as_str()) == Some("low"))
    {
        "low"
    } else {
        "none"
    };

    serde_json::json!({
        "overall": overall,
        "signals": signals,
    })
}

fn build_grounding_confidence_signal(
    entity_resolutions: &[crate::entity_linker::EntityResolution],
) -> serde_json::Value {
    let summary = summarize_grounding_confidence(entity_resolutions);
    let confirmation_options = entity_resolutions
        .iter()
        .filter(|resolution| {
            matches!(
                resolution.status,
                ResolutionStatus::Ambiguous | ResolutionStatus::SchemaCandidate
            )
        })
        .filter_map(|resolution| {
            let grounded = resolution
                .grounded_matches
                .iter()
                .map(|grounded| {
                    let label = grounded
                        .display_label
                        .as_deref()
                        .unwrap_or(&grounded.canonical_value);
                    serde_json::json!({
                        "kind": "backend_match",
                        "label": label,
                        "family_type": grounded.family_type,
                        "family_label": humanize_entity_family(&grounded.family_type),
                        "root_field": grounded.root_field,
                        "matched_field": grounded.matched_field,
                        "matched_value": grounded.matched_value,
                        "stable_key_field": grounded.stable_key_field,
                        "stable_key_value": grounded.stable_key_value,
                    })
                })
                .collect::<Vec<_>>();
            let options = if grounded.is_empty() {
                resolution
                    .schema_candidates
                    .iter()
                    .map(|candidate| {
                        serde_json::json!({
                            "kind": "schema_family",
                            "family_type": candidate.family_type,
                            "family_label": humanize_entity_family(&candidate.family_type),
                            "lookup_roots": candidate.lookup_roots,
                            "label_fields": candidate.label_fields,
                            "key_fields": candidate.key_fields,
                        })
                    })
                    .collect::<Vec<_>>()
            } else {
                grounded
            };

            (!options.is_empty()).then(|| {
                serde_json::json!({
                    "mention": resolution.mention,
                    "status": format!("{:?}", resolution.status),
                    "options": options,
                })
            })
        })
        .collect::<Vec<_>>();
    let grounded_entity_keys = entity_resolutions
        .iter()
        .filter(|resolution| matches!(resolution.status, ResolutionStatus::Grounded))
        .flat_map(|resolution| {
            resolution.grounded_matches.iter().map(|grounded| {
                serde_json::json!({
                    "mention": resolution.mention,
                    "family_type": grounded.family_type,
                    "root_field": grounded.root_field,
                    "matched_field": grounded.matched_field,
                    "matched_value": grounded.matched_value,
                    "stable_key_field": grounded.stable_key_field,
                    "stable_key_value": grounded.stable_key_value,
                    "display_label": grounded.display_label,
                })
            })
        })
        .collect::<Vec<_>>();
    let missing_stable_key_mentions = entity_resolutions
        .iter()
        .filter(|resolution| matches!(resolution.status, ResolutionStatus::Grounded))
        .filter(|resolution| {
            resolution.grounded_matches.iter().all(|grounded| {
                grounded.stable_key_field.is_none() || grounded.stable_key_value.is_none()
            })
        })
        .map(|resolution| resolution.mention.clone())
        .collect::<Vec<_>>();

    serde_json::json!({
        "overall": summary.overall,
        "clarification_recommended": summary.clarification_recommended,
        "grounded_count": summary.grounded_mentions.len(),
        "schema_candidate_count": summary.schema_candidate_mentions.len(),
        "ambiguous_count": summary.ambiguous_mentions.len(),
        "unresolved_count": summary.unresolved_mentions.len(),
        "grounded_mentions": summary.grounded_mentions,
        "schema_candidate_mentions": summary.schema_candidate_mentions,
        "ambiguous_mentions": summary.ambiguous_mentions,
        "unresolved_mentions": summary.unresolved_mentions,
        "mentions_requiring_confirmation": summary.mentions_requiring_confirmation,
        "confirmation_options": confirmation_options,
        "grounded_entity_keys": grounded_entity_keys,
        "missing_stable_key_mentions": missing_stable_key_mentions,
        "signals": summary.signals,
    })
}

fn merge_execution_groundings_into_resolutions(
    entity_resolutions: &[crate::entity_linker::EntityResolution],
    execution_groundings: &[ExecutionGrounding],
) -> Vec<crate::entity_linker::EntityResolution> {
    let mut merged = entity_resolutions.to_vec();
    for resolution in &mut merged {
        let matches = execution_groundings
            .iter()
            .filter(|grounding| grounding.mention.eq_ignore_ascii_case(&resolution.mention))
            .collect::<Vec<_>>();
        if matches.is_empty() {
            continue;
        }
        if matches.len() > 1 {
            resolution.status = ResolutionStatus::Ambiguous;
            resolution.notes.push(
                "Execution produced multiple exact entity matches for this mention.".to_string(),
            );
            continue;
        }
        let grounding = matches[0];
        resolution.status = ResolutionStatus::Grounded;
        resolution.grounded_matches = vec![GroundedEntityMatch {
            mention: grounding.mention.clone(),
            family_type: grounding.family_type.clone(),
            root_field: grounding.root_field.clone(),
            matched_field: grounding.matched_field.clone(),
            matched_value: grounding.matched_value.clone(),
            stable_key_field: grounding.stable_key_field.clone(),
            stable_key_value: grounding.stable_key_value.clone(),
            canonical_value: grounding.matched_value.clone(),
            display_label: grounding.display_label.clone(),
        }];
        resolution.notes.retain(|note| {
            !note.contains("No exact label match was found")
                && !note.contains("Backend grounding skipped")
                && !note.contains("No exact backend match was found")
                && !note.contains("confirmation is required")
        });
        if !resolution
            .notes
            .iter()
            .any(|note| note == "Execution resolved this mention via an exact scoped fetch.")
        {
            resolution
                .notes
                .push("Execution resolved this mention via an exact scoped fetch.".to_string());
        }
    }
    merged
}

fn build_claim_traceability(answer: &str, evidence: &ExecutionEvidence) -> serde_json::Value {
    let answer_lower = answer.to_ascii_lowercase();
    let mut value_to_fields: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();

    for (field, values) in &evidence.field_values {
        for value in values {
            let trimmed = value.trim();
            if trimmed.len() < 2 || trimmed.len() > 64 {
                continue;
            }
            if answer_lower.contains(&trimmed.to_ascii_lowercase()) {
                value_to_fields
                    .entry(trimmed.to_string())
                    .or_default()
                    .push(field.clone());
            }
        }
    }

    let claims = value_to_fields
        .into_iter()
        .take(50)
        .map(|(value, mut fields)| {
            fields.sort();
            fields.dedup();
            serde_json::json!({ "value": value, "fields": fields })
        })
        .collect::<Vec<_>>();

    serde_json::json!({
        "method": "string_match",
        "claims": claims
    })
}

fn matches_string_constraint(
    op: &str,
    constraint_values: &[String],
    evidence_values: &[String],
) -> bool {
    let constraint_lower = constraint_values
        .iter()
        .map(|v| v.to_ascii_lowercase())
        .collect::<Vec<_>>();
    let evidence_lower = evidence_values
        .iter()
        .map(|v| v.to_ascii_lowercase())
        .collect::<Vec<_>>();
    match op {
        "contains" | "like" => constraint_lower
            .iter()
            .any(|needle| evidence_lower.iter().any(|value| value.contains(needle))),
        "ne" => evidence_lower
            .iter()
            .all(|value| !constraint_lower.contains(value)),
        "in" => evidence_values.iter().any(|value| {
            constraint_values
                .iter()
                .any(|constraint| scope_values_match(constraint, value))
        }),
        _ => {
            evidence_lower
                .iter()
                .any(|value| constraint_lower.contains(value))
                || evidence_values.iter().any(|value| {
                    constraint_values
                        .iter()
                        .any(|constraint| scope_values_match(constraint, value))
                })
        }
    }
}

fn parse_numeric_value(value: &str) -> Option<f64> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed.parse::<f64>().ok()
}

fn parse_time_millis_from_str(value: &str) -> Option<i64> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(trimmed) {
        return Some(dt.timestamp_millis());
    }
    if let Ok(naive_dt) = chrono::NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%S") {
        return Some(naive_dt.and_utc().timestamp_millis());
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(trimmed, "%Y-%m-%d")
        && let Some(dt) = d.and_hms_opt(0, 0, 0)
    {
        return Some(dt.and_utc().timestamp_millis());
    }
    if let Ok(num) = trimmed.parse::<i64>() {
        if num.abs() >= 1_000_000_000_000 {
            return Some(num);
        }
        if num.abs() >= 1_000_000_000 {
            return Some(num * 1000);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> crate::schema_registry::SchemaRegistry {
        crate::schema_registry::SchemaRegistry::new(include_str!(
            "../schemas/consumer_schema.graphql"
        ))
    }

    #[test]
    fn schema_retrieval_summary_surfaces_confidence_and_roots() {
        let reg = registry();
        let budget = reg.planner_retrieval_budget("Show wind speed trend over time");
        let slice = reg.schema_retrieval_slice(
            "Show wind speed trend over time",
            budget.root_limit,
            budget.field_limit,
        );
        let summary = schema_retrieval_summary(&slice, budget);

        assert_eq!(summary["mode"], "retrieved");
        assert_eq!(summary["confidence"], "high");
        assert!(
            summary["roots"]
                .as_array()
                .is_some_and(|roots| roots.iter().any(|root| {
                    root.get("root").and_then(|value| value.as_str())
                        == Some("queryWeatherPrediction")
                })),
            "expected WeatherPrediction root in summary: {summary}"
        );
    }

    #[test]
    fn uncertainty_signal_surfaces_backend_relation_gap() {
        let deterministic = DeterministicAnswer {
            text: "I found the requested parent entity, but the backend returned no child rows."
                .to_string(),
            kind: DeterministicAnswerKind::Diagnostic,
        };
        let signal = build_uncertainty_signal(
            &[crate::planner::ExecutedArtifact::debug_log(
                "DEBUG_PREP_LOGS",
                "[STEP_OUTPUT] backend returned no child rows for queryOffshoreWindFarm.hasOffshoreWindTurbine",
            )],
            &serde_json::json!({"mode": "retrieved", "confidence": "high"}),
            &serde_json::json!({"clarification_recommended": false}),
            None,
            &deterministic,
            &deterministic.text,
            &serde_json::json!({"missing_constraints": []}),
        );

        assert_eq!(signal["overall"], "high");
        assert!(
            signal["signals"].as_array().is_some_and(|signals| {
                signals
                    .iter()
                    .any(|item| item["kind"] == "backend_relation_gap")
            }),
            "expected backend relation gap uncertainty: {signal}"
        );
    }

    #[test]
    fn uncertainty_signal_surfaces_repair_and_competitive_roots() {
        let deterministic = DeterministicAnswer {
            text: "Found 1 result.".to_string(),
            kind: DeterministicAnswerKind::RowList,
        };
        let signal = build_uncertainty_signal(
            &[crate::planner::ExecutedArtifact::debug_log(
                "DEBUG_PREP_LOGS",
                "[REPAIR] empty-rows retry; [REPAIR] next_query(deterministic)=query AutoIR",
            )],
            &serde_json::json!({
                "mode": "retrieved",
                "confidence": "medium",
                "competitive_root_count": 2
            }),
            &serde_json::json!({"clarification_recommended": false}),
            Some(r#"{"version":"v2","steps":[]}"#),
            &deterministic,
            &deterministic.text,
            &serde_json::json!({"missing_constraints": []}),
        );

        let kinds = signal["signals"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| item["kind"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(signal["overall"], "medium");
        assert!(kinds.contains(&"schema_retrieval_confidence"));
        assert!(kinds.contains(&"competitive_schema_roots"));
        assert!(kinds.contains(&"planner_repair_used"));
        assert!(kinds.contains(&"query_repair_or_fallback_used"));
    }

    #[test]
    fn planned_constraints_are_classified() {
        let reg = registry();
        let time_field = reg
            .root_time_filter_fields("queryHistoricalScadaAgg10min")
            .into_iter()
            .find(|field| {
                let lower = field.to_ascii_lowercase();
                !matches!(lower.as_str(), "and" | "or" | "not")
            })
            .expect("expected time filter field for queryHistoricalScadaAgg10min");
        let query = format!(
            "query {{
            queryHistoricalScadaAgg10min(filter: {{{time_field}: {{ge: \"2026-02-16\", le: \"2026-02-22\"}}}}) {{ {time_field} }}
            queryVessel(filter: {{name: {{eq: \"the wagon\"}}}}) {{ name }}
        }}"
        );
        let constraints = planned_constraints_from_query(&reg, query.as_str());
        assert!(
            constraints.iter().any(|c| {
                c.root == "queryHistoricalScadaAgg10min" && c.kind == ScopeConstraintKind::Time
            }),
            "expected time constraint classification (time_field={time_field}) in {constraints:?}"
        );
        assert!(
            constraints
                .iter()
                .any(|c| { c.root == "queryVessel" && c.kind == ScopeConstraintKind::Entity }),
            "expected entity constraint classification in {constraints:?}"
        );
    }

    #[test]
    fn evidence_time_values_clear_missing_time_scope() {
        let reg = registry();
        let time_field = reg
            .root_time_filter_fields("queryHistoricalScadaAgg10min")
            .into_iter()
            .find(|field| {
                let lower = field.to_ascii_lowercase();
                !matches!(lower.as_str(), "and" | "or" | "not")
            })
            .expect("expected time filter field for queryHistoricalScadaAgg10min");
        let query = format!(
            "query {{
            queryHistoricalScadaAgg10min(filter: {{{time_field}: {{ge: \"2026-02-16\", le: \"2026-02-22\"}}}}) {{ {time_field} }}
        }}"
        );
        let constraints = planned_constraints_from_query(&reg, query.as_str());
        let mut field_values = std::collections::HashMap::new();
        field_values.insert(time_field.clone(), vec!["2026-02-16".to_string()]);
        let evidence = ExecutionEvidence {
            row_count: 1,
            sample_rows: vec![],
            literals: vec![],
            time_values: vec![1_771_200_000_000], // 2026-02-16T00:00:00Z
            field_values,
        };
        let missing = vec![ScopeConstraint {
            root: "queryHistoricalScadaAgg10min".to_string(),
            field: time_field,
            op: Some("ge".to_string()),
            values: vec!["2026-02-16".to_string()],
        }];
        let (remaining, matched) =
            reconcile_missing_with_evidence(&missing, &constraints, &evidence);
        assert!(
            remaining.is_empty(),
            "expected time constraint to be matched by evidence; constraints={constraints:?}, remaining={remaining:?}, matched={matched:?}"
        );
        assert_eq!(matched.len(), 1);
    }

    #[test]
    fn evidence_requires_field_match_for_scope() {
        let reg = registry();
        let query = "query { queryVessel(filter: { name: { eq: \"V1\" } }) { name } }";
        let constraints = planned_constraints_from_query(&reg, query);
        let mut field_values = std::collections::HashMap::new();
        field_values.insert("other".to_string(), vec!["V1".to_string()]);
        let evidence = ExecutionEvidence {
            row_count: 1,
            sample_rows: vec![],
            literals: vec!["V1".to_string()],
            time_values: vec![],
            field_values,
        };
        let missing = vec![ScopeConstraint {
            root: "queryVessel".to_string(),
            field: "name".to_string(),
            op: Some("eq".to_string()),
            values: vec!["V1".to_string()],
        }];
        let (remaining, matched) =
            reconcile_missing_with_evidence(&missing, &constraints, &evidence);
        assert_eq!(matched.len(), 0);
        assert_eq!(remaining.len(), 1);
    }

    #[test]
    fn evidence_matches_spaced_identifier_scope_variants() {
        let reg = registry();
        let query = "query { queryTag(filter: { plantId: { eq: \"PLANT-2\" } }) { plantId } }";
        let constraints = planned_constraints_from_query(&reg, query);
        let mut field_values = std::collections::HashMap::new();
        field_values.insert("plantId".to_string(), vec!["PLANT-  2".to_string()]);
        let evidence = ExecutionEvidence {
            row_count: 1,
            sample_rows: vec![],
            literals: vec!["PLANT-  2".to_string()],
            time_values: vec![],
            field_values,
        };
        let missing = vec![ScopeConstraint {
            root: "queryTag".to_string(),
            field: "plantId".to_string(),
            op: Some("eq".to_string()),
            values: vec!["PLANT-2".to_string()],
        }];
        let (remaining, matched) =
            reconcile_missing_with_evidence(&missing, &constraints, &evidence);
        assert!(
            remaining.is_empty(),
            "expected spaced identifier evidence to preserve scope; remaining={remaining:?}, matched={matched:?}"
        );
        assert_eq!(matched.len(), 1);
    }

    #[test]
    fn entity_scope_detection_requires_alpha_identifier() {
        let schema = SchemaRegistry::new(include_str!("../schemas/consumer_schema.graphql"));
        assert!(user_has_entity_scope_request(
            &schema,
            r#"List turbines in wind farm "Wind Farm 1"."#
        ));
        assert!(!user_has_entity_scope_request(
            &schema,
            "List turbines in wind farm Wind Farm 1."
        ));
        assert!(user_has_entity_scope_request(
            &schema,
            "Show turbine T3 details."
        ));
        assert!(!user_has_entity_scope_request(
            &schema,
            "Top 5 turbines by accumulatedDowntime."
        ));
    }

    #[test]
    fn backend_placeholder_scope_issue_explains_rejected_source_field() {
        let plan = crate::planner::MultiStepPlan {
            rewrites: String::new(),
            notes: String::new(),
            execute_error: String::new(),
            steps: vec![
                crate::planner::ExecutableStep {
                    id: "s1".to_string(),
                    description: "fetch farm".to_string(),
                    query: None,
                    op: crate::planner::PlanV2Op::Fetch {
                        root_field: "queryOffshoreWindFarm".to_string(),
                        fields: vec!["name".to_string(), "locationId".to_string()],
                        filter: None,
                        order: None,
                        first: Some(1),
                        offset: None,
                    },
                },
                crate::planner::ExecutableStep {
                    id: "s2".to_string(),
                    description: "fetch turbines".to_string(),
                    query: None,
                    op: crate::planner::PlanV2Op::Fetch {
                        root_field: "queryOffshoreWindTurbine".to_string(),
                        fields: vec!["name".to_string(), "shortName".to_string()],
                        filter: None,
                        order: None,
                        first: Some(10),
                        offset: None,
                    },
                },
            ],
        };
        let missing = vec![ScopeConstraint {
            root: "queryOffshoreWindTurbine".to_string(),
            field: "locationId".to_string(),
            op: Some("eq".to_string()),
            values: vec!["${s1.locationId}".to_string()],
        }];
        let effective_queries = vec![crate::planner::ExecutedArtifact::debug_log(
            "DEBUG_PREP_LOGS",
            "[QUERY_REPAIR_TRACE] s1:\n[REPAIR] graphql execution errors=GraphQL execution errors: Unknown field \"locationId\" on type \"OffshoreWindFarm\".\n[REPAIR] deterministic_retry=| Applied deterministic invalid selected-field pruning and retrying.\n",
        )];

        let message = backend_placeholder_scope_issue_message(&missing, &plan, &effective_queries)
            .expect("expected backend placeholder message");

        assert!(message.contains("queryOffshoreWindFarm.locationId"));
        assert!(message.contains("queryOffshoreWindTurbine.locationId eq ${s1.locationId}"));
        assert!(message.contains("backend/schema mismatch"));
    }

    #[test]
    fn backend_placeholder_scope_issue_ignores_non_backend_missing_scope() {
        let plan = crate::planner::MultiStepPlan {
            rewrites: String::new(),
            notes: String::new(),
            execute_error: String::new(),
            steps: vec![crate::planner::ExecutableStep {
                id: "s1".to_string(),
                description: "fetch turbines".to_string(),
                query: None,
                op: crate::planner::PlanV2Op::Fetch {
                    root_field: "queryOffshoreWindTurbine".to_string(),
                    fields: vec!["name".to_string()],
                    filter: None,
                    order: None,
                    first: Some(10),
                    offset: None,
                },
            }],
        };
        let missing = vec![ScopeConstraint {
            root: "queryOffshoreWindTurbine".to_string(),
            field: "name".to_string(),
            op: Some("eq".to_string()),
            values: vec!["T3".to_string()],
        }];
        let effective_queries = vec![crate::planner::ExecutedArtifact::debug_log(
            "DEBUG_PREP_LOGS",
            "[QUERY_REPAIR_TRACE] s1:\n[REPAIR] success\n",
        )];

        assert!(
            backend_placeholder_scope_issue_message(&missing, &plan, &effective_queries).is_none()
        );
    }

    #[test]
    fn location_label_capability_gap_detects_identifier_only_zero_row_match() {
        let scope_used = serde_json::json!({
            "matched_constraints": [
                {
                    "root": "queryOffshoreWindTurbine",
                    "field": "sapLocationId",
                    "op": "eq",
                    "values": ["Farm Zone 3"]
                }
            ]
        });
        let evidence = ExecutionEvidence {
            row_count: 0,
            sample_rows: vec![],
            literals: vec![],
            time_values: vec![],
            field_values: std::collections::HashMap::new(),
        };
        let bootstrap = crate::schema_registry::SchemaRegistry::new(include_str!(
            "../schemas/consumer_schema.graphql"
        ));
        let sls = crate::sls::load_sls_merged(&bootstrap, "sls.yaml").expect("expected sls");

        let guard = location_label_capability_gap(
            "List turbines in location \"Farm Zone 3\"",
            &scope_used,
            &evidence,
            Some(&sls),
        );

        assert!(guard.is_some());
    }

    #[test]
    fn execution_grounding_overrides_stale_ambiguous_resolution() {
        let resolutions = vec![crate::entity_linker::EntityResolution {
            mention: "the wagon".to_string(),
            status: ResolutionStatus::Ambiguous,
            grounded_matches: vec![],
            schema_candidates: vec![],
            notes: vec![
                "No exact label match was found on the schema-supported roots probed for this mention."
                    .to_string(),
                "Backend found one exact label candidate on schema-supported roots; user confirmation is required before execution."
                    .to_string(),
            ],
        }];
        let execution_groundings = vec![ExecutionGrounding {
            mention: "the wagon".to_string(),
            family_type: "Vessel".to_string(),
            root_field: "queryVessel".to_string(),
            matched_field: "name".to_string(),
            matched_value: "the wagon".to_string(),
            stable_key_field: Some("mmsi".to_string()),
            stable_key_value: Some("123456789".to_string()),
            display_label: Some("the wagon".to_string()),
        }];

        let merged =
            merge_execution_groundings_into_resolutions(&resolutions, &execution_groundings);

        assert_eq!(merged.len(), 1);
        assert!(matches!(merged[0].status, ResolutionStatus::Grounded));
        assert_eq!(merged[0].grounded_matches.len(), 1);
        assert_eq!(merged[0].grounded_matches[0].root_field, "queryVessel");
        assert!(
            merged[0]
                .notes
                .iter()
                .any(|note| note.contains("Execution resolved this mention"))
        );
        assert!(
            merged[0]
                .notes
                .iter()
                .all(|note| !note.contains("No exact label match was found"))
        );
        assert!(
            merged[0]
                .notes
                .iter()
                .all(|note| !note.contains("confirmation is required"))
        );
    }

    #[test]
    fn grounding_confidence_signal_requests_clarification_for_ambiguous_mentions() {
        let resolutions = vec![crate::entity_linker::EntityResolution {
            mention: "Wind Farm 3".to_string(),
            status: ResolutionStatus::Ambiguous,
            grounded_matches: vec![],
            schema_candidates: vec![],
            notes: vec![],
        }];

        let signal = build_grounding_confidence_signal(&resolutions);

        assert_eq!(
            signal.get("overall").and_then(|v| v.as_str()),
            Some("clarification_needed")
        );
        assert_eq!(
            signal
                .get("clarification_recommended")
                .and_then(|v| v.as_bool()),
            Some(true)
        );
        assert!(
            signal
                .get("ambiguous_mentions")
                .and_then(|v| v.as_array())
                .is_some_and(|arr| arr.iter().any(|v| v.as_str() == Some("Wind Farm 3"))),
            "expected ambiguous mention in signal: {signal}"
        );
    }

    #[test]
    fn grounding_confidence_signal_is_high_for_fully_grounded_mentions() {
        let resolutions = vec![crate::entity_linker::EntityResolution {
            mention: "the wagon".to_string(),
            status: ResolutionStatus::Grounded,
            grounded_matches: vec![GroundedEntityMatch {
                mention: "the wagon".to_string(),
                family_type: "Vessel".to_string(),
                root_field: "queryVessel".to_string(),
                matched_field: "name".to_string(),
                matched_value: "the wagon".to_string(),
                stable_key_field: Some("mmsi".to_string()),
                stable_key_value: Some("123456789".to_string()),
                canonical_value: "the wagon".to_string(),
                display_label: Some("the wagon".to_string()),
            }],
            schema_candidates: vec![],
            notes: vec![],
        }];

        let signal = build_grounding_confidence_signal(&resolutions);

        assert_eq!(
            signal.get("overall").and_then(|v| v.as_str()),
            Some("high_confidence")
        );
        assert_eq!(
            signal
                .get("clarification_recommended")
                .and_then(|v| v.as_bool()),
            Some(false)
        );
        assert_eq!(
            signal.get("grounded_count").and_then(|v| v.as_u64()),
            Some(1)
        );
        assert_eq!(
            signal
                .get("confirmation_options")
                .and_then(|value| value.as_array())
                .map(Vec::len),
            Some(0)
        );
        let grounded_keys = signal
            .get("grounded_entity_keys")
            .and_then(|v| v.as_array())
            .expect("expected grounded entity key details");
        assert_eq!(
            grounded_keys
                .first()
                .and_then(|value| value.get("stable_key_field"))
                .and_then(|value| value.as_str()),
            Some("mmsi")
        );
        assert_eq!(
            grounded_keys
                .first()
                .and_then(|value| value.get("stable_key_value"))
                .and_then(|value| value.as_str()),
            Some("123456789")
        );
    }

    #[test]
    fn grounding_clarification_message_lists_ambiguous_options() {
        let schema = registry();
        let resolutions = vec![crate::entity_linker::EntityResolution {
            mention: "Wind Farm 3".to_string(),
            status: ResolutionStatus::Ambiguous,
            grounded_matches: vec![
                GroundedEntityMatch {
                    mention: "Wind Farm 3".to_string(),
                    family_type: "OffshoreWindFarm".to_string(),
                    root_field: "queryOffshoreWindFarm".to_string(),
                    matched_field: "name".to_string(),
                    matched_value: "Wind Farm 3".to_string(),
                    stable_key_field: Some("locationUid".to_string()),
                    stable_key_value: Some("FARM-003".to_string()),
                    canonical_value: "Wind Farm 3".to_string(),
                    display_label: Some("Wind Farm 3".to_string()),
                },
                GroundedEntityMatch {
                    mention: "Wind Farm 3".to_string(),
                    family_type: "OffshoreSubstation".to_string(),
                    root_field: "queryOffshoreSubstation".to_string(),
                    matched_field: "name".to_string(),
                    matched_value: "Wind Farm 3".to_string(),
                    stable_key_field: Some("locationUid".to_string()),
                    stable_key_value: Some("OSS-003".to_string()),
                    canonical_value: "Wind Farm 3".to_string(),
                    display_label: Some("Wind Farm 3".to_string()),
                },
            ],
            schema_candidates: vec![],
            notes: vec![],
        }];

        let message = build_grounding_clarification_message(
            &schema,
            "What does Wind Farm 3 refer to?",
            &resolutions,
        )
        .expect("expected clarification message for ambiguous resolution");

        assert!(
            message.contains("multiple possible entity matches"),
            "expected ambiguity lead-in: {message}"
        );
        assert!(
            message.contains("`Wind Farm 3` could refer to"),
            "expected mention detail in message: {message}"
        );
        assert!(
            message.contains("offshore wind farm"),
            "expected humanized family type in message: {message}"
        );
        assert!(
            message.contains("offshore substation"),
            "expected second humanized family type in message: {message}"
        );
    }

    #[test]
    fn grounding_clarification_message_requests_confirmation_for_schema_candidates() {
        let schema = registry();
        let resolutions = vec![crate::entity_linker::EntityResolution {
            mention: "Alpha".to_string(),
            status: ResolutionStatus::SchemaCandidate,
            grounded_matches: vec![],
            schema_candidates: vec![
                crate::entity_linker::SchemaEntityCandidate {
                    family_type: "OffshoreWindFarm".to_string(),
                    lookup_roots: vec!["queryOffshoreWindFarm".to_string()],
                    key_fields: vec!["shortName".to_string()],
                    label_fields: vec!["name".to_string()],
                    filter_fields: vec![],
                },
                crate::entity_linker::SchemaEntityCandidate {
                    family_type: "OnshoreSubstation".to_string(),
                    lookup_roots: vec!["queryOnshoreSubstation".to_string()],
                    key_fields: vec!["shortName".to_string()],
                    label_fields: vec!["name".to_string()],
                    filter_fields: vec![],
                },
            ],
            notes: vec![],
        }];

        let message = build_grounding_clarification_message(
            &schema,
            "What does Alpha refer to?",
            &resolutions,
        )
        .expect("expected clarification message for schema-only candidates");

        assert!(
            message.contains("not enough grounding to choose confidently"),
            "expected confirmation lead-in: {message}"
        );
        assert!(
            message.contains("`Alpha` looks like"),
            "expected mention detail in message: {message}"
        );
        assert!(
            message.contains("offshore wind farm"),
            "expected first candidate family in message: {message}"
        );
        assert!(
            message.contains("onshore substation"),
            "expected second candidate family in message: {message}"
        );
    }

    #[test]
    fn grounding_clarification_message_is_absent_for_high_confidence_grounding() {
        let schema = registry();
        let resolutions = vec![crate::entity_linker::EntityResolution {
            mention: "the wagon".to_string(),
            status: ResolutionStatus::Grounded,
            grounded_matches: vec![GroundedEntityMatch {
                mention: "the wagon".to_string(),
                family_type: "Vessel".to_string(),
                root_field: "queryVessel".to_string(),
                matched_field: "name".to_string(),
                matched_value: "the wagon".to_string(),
                stable_key_field: Some("mmsi".to_string()),
                stable_key_value: Some("123456789".to_string()),
                canonical_value: "the wagon".to_string(),
                display_label: Some("the wagon".to_string()),
            }],
            schema_candidates: vec![],
            notes: vec![],
        }];

        assert!(
            build_grounding_clarification_message(&schema, "Where is the wagon?", &resolutions)
                .is_none(),
            "did not expect clarification message for grounded resolution"
        );
    }

    #[test]
    fn strong_single_backend_candidate_is_promoted_to_grounded() {
        let schema = registry();
        let mut resolutions = vec![crate::entity_linker::EntityResolution {
            mention: "Wind Farm 1".to_string(),
            status: ResolutionStatus::SchemaCandidate,
            grounded_matches: vec![GroundedEntityMatch {
                mention: "Wind Farm 1".to_string(),
                family_type: "OffshoreWindFarm".to_string(),
                root_field: "queryOffshoreWindFarm".to_string(),
                matched_field: "name".to_string(),
                matched_value: "Wind Farm 1".to_string(),
                stable_key_field: Some("name".to_string()),
                stable_key_value: Some("Wind Farm 1".to_string()),
                canonical_value: "Wind Farm 1".to_string(),
                display_label: Some("Wind Farm 1".to_string()),
            }],
            schema_candidates: vec![crate::entity_linker::SchemaEntityCandidate {
                family_type: "OffshoreWindFarm".to_string(),
                lookup_roots: vec!["queryOffshoreWindFarm".to_string()],
                key_fields: vec!["name".to_string(), "shortName".to_string()],
                label_fields: vec!["name".to_string()],
                filter_fields: vec![],
            }],
            notes: vec![
                "Backend found one exact label candidate on schema-supported roots; user confirmation is required before execution."
                    .to_string(),
            ],
        }];

        promote_strong_single_backend_candidates(
            &schema,
            "List turbines in wind farm Wind Farm 1.",
            &mut resolutions,
        );

        assert_eq!(resolutions[0].status, ResolutionStatus::Grounded);
        assert!(
            resolutions[0]
                .notes
                .iter()
                .all(|note| !note.contains("confirmation is required")),
            "promoted grounded candidate should not retain stale confirmation-required notes"
        );
        assert!(
            build_grounding_clarification_message(
                &schema,
                "List turbines in wind farm Wind Farm 1.",
                &resolutions
            )
            .is_none(),
            "did not expect confirmation after a strongly typed exact backend match"
        );
    }

    #[test]
    fn rank_source_fetches_gain_schema_role_display_fields() {
        let schema = registry();
        let mut plan = crate::planner::PlanV2 {
            version: Some("v2".to_string()),
            rewrites: vec![],
            notes: vec![],
            steps: vec![
                crate::planner::PlanV2Step {
                    id: "s1".to_string(),
                    op: crate::planner::PlanV2Op::Fetch {
                        root_field: "queryOffshoreWindFarm".to_string(),
                        fields: vec!["name".to_string(), "ratedCapacity".to_string()],
                        first: Some(2000),
                        offset: None,
                        filter: None,
                        order: None,
                    },
                },
                crate::planner::PlanV2Step {
                    id: "s2".to_string(),
                    op: crate::planner::PlanV2Op::Rank {
                        source: "s1".to_string(),
                        by: "ratedCapacity".to_string(),
                        direction: Some("asc".to_string()),
                        limit: Some(2),
                    },
                },
            ],
        };

        append_rank_source_display_fields(&mut plan, &schema);

        let crate::planner::PlanV2Op::Fetch { fields, .. } = &plan.steps[0].op else {
            panic!("expected fetch step");
        };
        assert!(
            fields.iter().any(|field| field == "shortName"),
            "expected schema/SLS role display field to be added for ranked source: {fields:?}"
        );
        assert!(
            plan.notes
                .iter()
                .any(|note| note.contains("rank_source_display_fields_added")),
            "expected augmentation note"
        );
    }

    #[test]
    fn grounding_clarification_message_is_absent_for_typed_single_family_schema_candidates() {
        let schema = registry();
        let resolutions = vec![crate::entity_linker::EntityResolution {
            mention: "Wind Farm 1".to_string(),
            status: ResolutionStatus::SchemaCandidate,
            grounded_matches: vec![],
            schema_candidates: vec![crate::entity_linker::SchemaEntityCandidate {
                family_type: "OffshoreWindFarm".to_string(),
                lookup_roots: vec!["queryOffshoreWindFarm".to_string()],
                key_fields: vec!["shortName".to_string()],
                label_fields: vec!["name".to_string()],
                filter_fields: vec![],
            }],
            notes: vec![],
        }];

        assert!(
            build_grounding_clarification_message(
                &schema,
                "Show details for wind farm Wind Farm 1.",
                &resolutions
            )
            .is_none(),
            "did not expect clarification when the prompt already pins the family strongly"
        );
    }

    #[test]
    fn grounding_clarification_message_requests_confirmation_for_backend_candidate() {
        let schema = registry();
        let resolutions = vec![crate::entity_linker::EntityResolution {
            mention: "turbine 115".to_string(),
            status: ResolutionStatus::SchemaCandidate,
            grounded_matches: vec![GroundedEntityMatch {
                mention: "turbine 115".to_string(),
                family_type: "OffshoreWindTurbine".to_string(),
                root_field: "queryOffshoreWindTurbine".to_string(),
                matched_field: "name".to_string(),
                matched_value: "Turbine 115".to_string(),
                stable_key_field: None,
                stable_key_value: None,
                canonical_value: "Turbine 115".to_string(),
                display_label: Some("Turbine 115".to_string()),
            }],
            schema_candidates: vec![crate::entity_linker::SchemaEntityCandidate {
                family_type: "OffshoreWindTurbine".to_string(),
                lookup_roots: vec!["queryOffshoreWindTurbine".to_string()],
                key_fields: vec!["shortName".to_string()],
                label_fields: vec!["name".to_string()],
                filter_fields: vec![],
            }],
            notes: vec![],
        }];

        let message = build_grounding_clarification_message(
            &schema,
            r#"Compare average downtime for "turbine 115"."#,
            &resolutions,
        )
        .expect("expected confirmation message for backend-grounded descriptive mention");

        assert!(
            message.contains("`turbine 115` looks like"),
            "expected candidate mention detail: {message}"
        );
        assert!(
            message.contains("`Turbine 115` (offshore wind turbine)"),
            "expected backend-grounded option in message: {message}"
        );

        let signal = build_grounding_confidence_signal(&resolutions);
        let options = signal
            .get("confirmation_options")
            .and_then(|value| value.as_array())
            .expect("expected structured confirmation options");
        assert_eq!(options.len(), 1);
        assert_eq!(
            options[0]
                .get("options")
                .and_then(|value| value.as_array())
                .and_then(|items| items.first())
                .and_then(|option| option.get("label"))
                .and_then(|value| value.as_str()),
            Some("Turbine 115")
        );
    }

    #[test]
    fn anchored_query_roots_expand_grounded_wind_farm_to_neighbor_query_roots() {
        let schema = registry();
        let resolutions = vec![crate::entity_linker::EntityResolution {
            mention: "Wind Farm 1".to_string(),
            status: ResolutionStatus::Grounded,
            grounded_matches: vec![GroundedEntityMatch {
                mention: "Wind Farm 1".to_string(),
                family_type: "OffshoreWindFarm".to_string(),
                root_field: "queryOffshoreWindFarm".to_string(),
                matched_field: "name".to_string(),
                matched_value: "Wind Farm 1".to_string(),
                stable_key_field: Some("shortName".to_string()),
                stable_key_value: Some("WF1".to_string()),
                canonical_value: "Wind Farm 1".to_string(),
                display_label: Some("Wind Farm 1".to_string()),
            }],
            schema_candidates: vec![],
            notes: vec![],
        }];

        let roots = anchored_query_roots(&schema, &resolutions);

        assert_eq!(
            roots.first().map(String::as_str),
            Some("queryOffshoreWindFarm"),
            "expected anchored roots to lead with the grounded family root, got {roots:?}"
        );
        assert!(
            roots.iter().any(|root| root == "queryOffshoreWindTurbine"),
            "expected farm anchor to expand to turbine neighbor root, got {roots:?}"
        );
    }

    #[test]
    fn anchored_query_roots_use_schema_candidate_lookup_roots_before_fallback_retrieval() {
        let schema = registry();
        let resolutions = vec![crate::entity_linker::EntityResolution {
            mention: "OSS3".to_string(),
            status: ResolutionStatus::SchemaCandidate,
            grounded_matches: vec![],
            schema_candidates: vec![crate::entity_linker::SchemaEntityCandidate {
                family_type: "OffshoreSubstation".to_string(),
                lookup_roots: vec![
                    "batchGetOffshoreSubstation".to_string(),
                    "queryOffshoreSubstation".to_string(),
                ],
                key_fields: vec!["shortName".to_string()],
                label_fields: vec!["name".to_string()],
                filter_fields: vec!["shortName".to_string()],
            }],
            notes: vec![],
        }];

        let roots = anchored_query_roots(&schema, &resolutions);

        assert!(
            roots.iter().any(|root| root == "queryOffshoreSubstation"),
            "expected schema-candidate lookup roots to anchor planner context, got {roots:?}"
        );
        assert!(
            !roots
                .iter()
                .any(|root| root == "batchGetOffshoreSubstation"),
            "did not expect non-query lookup roots in planner prompt roots, got {roots:?}"
        );
    }

    #[test]
    fn compare_contains_antipattern_is_rejected_for_explicit_entities() {
        let schema = registry();
        let plan = crate::planner::PlanV2 {
            version: Some("v2".to_string()),
            rewrites: vec![],
            notes: vec![],
            steps: vec![
                crate::planner::PlanV2Step {
                    id: "s1".to_string(),
                    op: crate::planner::PlanV2Op::Fetch {
                        root_field: "queryOffshoreWindTurbine".to_string(),
                        fields: vec!["shortName".to_string(), "accumulatedDowntime".to_string()],
                        first: Some(2000),
                        offset: None,
                        filter: None,
                        order: None,
                    },
                },
                crate::planner::PlanV2Step {
                    id: "s2".to_string(),
                    op: crate::planner::PlanV2Op::FilterRows {
                        source: "s1".to_string(),
                        field: "shortName".to_string(),
                        operator: "contains".to_string(),
                        value: serde_json::json!("115"),
                    },
                },
                crate::planner::PlanV2Step {
                    id: "s3".to_string(),
                    op: crate::planner::PlanV2Op::FilterRows {
                        source: "s1".to_string(),
                        field: "shortName".to_string(),
                        operator: "contains".to_string(),
                        value: serde_json::json!("109"),
                    },
                },
                crate::planner::PlanV2Step {
                    id: "s4".to_string(),
                    op: crate::planner::PlanV2Op::Compare {
                        left: "s2".to_string(),
                        right: "s3".to_string(),
                        metric: Some(crate::planner::MetricSpec::Avg {
                            field: "accumulatedDowntime".to_string(),
                        }),
                    },
                },
            ],
        };

        let issue = compare_contains_antipattern_error(
            &plan,
            &schema,
            &["turbine 115".to_string(), "turbine 109".to_string()],
        );

        assert!(
            issue.is_some(),
            "expected anti-pattern issue for broad compare"
        );
    }

    #[test]
    fn compare_with_scoped_fetches_is_not_rejected() {
        let schema = registry();
        let plan = crate::planner::PlanV2 {
            version: Some("v2".to_string()),
            rewrites: vec![],
            notes: vec![],
            steps: vec![
                crate::planner::PlanV2Step {
                    id: "s1".to_string(),
                    op: crate::planner::PlanV2Op::Fetch {
                        root_field: "queryOffshoreWindTurbine".to_string(),
                        fields: vec!["shortName".to_string(), "accumulatedDowntime".to_string()],
                        first: Some(2),
                        offset: None,
                        filter: Some(serde_json::json!({
                            "name": { "eq": "Turbine 115" }
                        })),
                        order: None,
                    },
                },
                crate::planner::PlanV2Step {
                    id: "s2".to_string(),
                    op: crate::planner::PlanV2Op::Fetch {
                        root_field: "queryOffshoreWindTurbine".to_string(),
                        fields: vec!["shortName".to_string(), "accumulatedDowntime".to_string()],
                        first: Some(2),
                        offset: None,
                        filter: Some(serde_json::json!({
                            "name": { "eq": "Turbine 109" }
                        })),
                        order: None,
                    },
                },
                crate::planner::PlanV2Step {
                    id: "s3".to_string(),
                    op: crate::planner::PlanV2Op::Compare {
                        left: "s1".to_string(),
                        right: "s2".to_string(),
                        metric: Some(crate::planner::MetricSpec::Avg {
                            field: "accumulatedDowntime".to_string(),
                        }),
                    },
                },
            ],
        };

        let issue = compare_contains_antipattern_error(
            &plan,
            &schema,
            &["turbine 115".to_string(), "turbine 109".to_string()],
        );

        assert!(issue.is_none(), "unexpected anti-pattern issue: {issue:?}");
    }

    #[test]
    fn collect_post_fetch_scope_constraints_includes_filter_rows_steps() {
        let plan = crate::planner::MultiStepPlan {
            rewrites: String::new(),
            notes: String::new(),
            execute_error: String::new(),
            steps: vec![
                crate::planner::ExecutableStep {
                    id: "s1".to_string(),
                    description: "fetch".to_string(),
                    query: Some(
                        "query AutoIR { queryOffshoreWindTurbine { shortName } }".to_string(),
                    ),
                    op: crate::planner::PlanV2Op::Fetch {
                        root_field: "queryOffshoreWindTurbine".to_string(),
                        fields: vec!["shortName".to_string()],
                        first: Some(2000),
                        offset: None,
                        filter: None,
                        order: None,
                    },
                },
                crate::planner::ExecutableStep {
                    id: "s2".to_string(),
                    description: "filter".to_string(),
                    query: None,
                    op: crate::planner::PlanV2Op::FilterRows {
                        source: "s1".to_string(),
                        field: "shortName".to_string(),
                        operator: "contains".to_string(),
                        value: serde_json::json!("115"),
                    },
                },
            ],
        };

        let constraints = collect_post_fetch_scope_constraints(&plan);

        assert_eq!(constraints.len(), 1);
        assert_eq!(constraints[0].root, "queryOffshoreWindTurbine");
        assert_eq!(constraints[0].field, "shortName");
        assert_eq!(constraints[0].op.as_deref(), Some("contains"));
        assert_eq!(constraints[0].values, vec!["115".to_string()]);
    }

    #[test]
    fn semantic_repair_guard_detects_dropped_distance_haversine() {
        let original = crate::planner::PlanV2 {
            version: Some("v2".to_string()),
            rewrites: vec![],
            notes: vec![],
            steps: vec![
                crate::planner::PlanV2Step {
                    id: "s1".to_string(),
                    op: crate::planner::PlanV2Op::Fetch {
                        root_field: "queryVessel".to_string(),
                        fields: vec!["name".to_string()],
                        first: Some(100),
                        offset: None,
                        filter: Some(serde_json::json!({"name": {"eq": "the wagon"}})),
                        order: None,
                    },
                },
                crate::planner::PlanV2Step {
                    id: "s2".to_string(),
                    op: crate::planner::PlanV2Op::Fetch {
                        root_field: "queryOffshoreWindTurbine".to_string(),
                        fields: vec!["shortName".to_string()],
                        first: Some(100),
                        offset: None,
                        filter: Some(serde_json::json!({"shortName": {"eq": "T3"}})),
                        order: None,
                    },
                },
                crate::planner::PlanV2Step {
                    id: "s3".to_string(),
                    op: crate::planner::PlanV2Op::DistanceHaversine {
                        vessels_source: "s1".to_string(),
                        target_source: "s2".to_string(),
                    },
                },
            ],
        };
        let repaired = crate::planner::PlanV2 {
            version: Some("v2".to_string()),
            rewrites: vec![],
            notes: vec![],
            steps: vec![crate::planner::PlanV2Step {
                id: "s1".to_string(),
                op: crate::planner::PlanV2Op::Fetch {
                    root_field: "queryOffshoreWindTurbine".to_string(),
                    fields: vec!["shortName".to_string(), "name".to_string()],
                    first: Some(100),
                    offset: None,
                    filter: Some(serde_json::json!({"shortName": {"eq": "T3"}})),
                    order: None,
                },
            }],
        };

        let message = semantic_repair_guard_message(&original, &repaired)
            .expect("expected semantic repair guard");
        assert!(message.contains("distance calculation"));
        assert!(message.contains("could not support"));
    }

    #[test]
    fn semantic_repair_guard_allows_repair_that_keeps_required_ops() {
        let original = crate::planner::PlanV2 {
            version: Some("v2".to_string()),
            rewrites: vec![],
            notes: vec![],
            steps: vec![
                crate::planner::PlanV2Step {
                    id: "s1".to_string(),
                    op: crate::planner::PlanV2Op::Fetch {
                        root_field: "queryTag".to_string(),
                        fields: vec!["id".to_string()],
                        first: Some(2000),
                        offset: None,
                        filter: Some(serde_json::json!({"plantId": {"eq": "PLANT-005"}})),
                        order: None,
                    },
                },
                crate::planner::PlanV2Step {
                    id: "s2".to_string(),
                    op: crate::planner::PlanV2Op::Aggregate {
                        source: "s1".to_string(),
                        group_by: vec![],
                        metrics: vec![crate::planner::MetricSpec::Count],
                    },
                },
            ],
        };

        assert!(semantic_repair_guard_message(&original, &original).is_none());
    }

    #[test]
    fn semantic_repair_guard_detects_dropped_fetch_filter_without_filter_rows() {
        let original = crate::planner::PlanV2 {
            version: Some("v2".to_string()),
            rewrites: vec![],
            notes: vec![],
            steps: vec![crate::planner::PlanV2Step {
                id: "s1".to_string(),
                op: crate::planner::PlanV2Op::Fetch {
                    root_field: "queryOffshoreWindTurbine".to_string(),
                    fields: vec![
                        "name".to_string(),
                        "shortName".to_string(),
                        "accumulatedDowntime".to_string(),
                    ],
                    first: Some(2000),
                    offset: None,
                    filter: Some(serde_json::json!({
                        "accumulatedDowntime": { "gt": 400 }
                    })),
                    order: None,
                },
            }],
        };
        let repaired = crate::planner::PlanV2 {
            version: Some("v2".to_string()),
            rewrites: vec![],
            notes: vec![],
            steps: vec![crate::planner::PlanV2Step {
                id: "s1".to_string(),
                op: crate::planner::PlanV2Op::Fetch {
                    root_field: "queryOffshoreWindTurbine".to_string(),
                    fields: vec![
                        "name".to_string(),
                        "shortName".to_string(),
                        "accumulatedDowntime".to_string(),
                    ],
                    first: Some(2000),
                    offset: None,
                    filter: None,
                    order: None,
                },
            }],
        };

        let message = semantic_repair_guard_message(&original, &repaired)
            .expect("expected dropped filter guard");
        assert!(message.contains("accumulatedDowntime"));
        assert!(message.contains("gt"));
        assert!(message.contains("400"));
    }

    #[test]
    fn semantic_repair_guard_allows_fetch_filter_rewritten_as_filter_rows() {
        let original = crate::planner::PlanV2 {
            version: Some("v2".to_string()),
            rewrites: vec![],
            notes: vec![],
            steps: vec![crate::planner::PlanV2Step {
                id: "s1".to_string(),
                op: crate::planner::PlanV2Op::Fetch {
                    root_field: "queryOffshoreWindTurbine".to_string(),
                    fields: vec![
                        "name".to_string(),
                        "shortName".to_string(),
                        "accumulatedDowntime".to_string(),
                    ],
                    first: Some(2000),
                    offset: None,
                    filter: Some(serde_json::json!({
                        "accumulatedDowntime": { "gt": 400 }
                    })),
                    order: None,
                },
            }],
        };
        let repaired = crate::planner::PlanV2 {
            version: Some("v2".to_string()),
            rewrites: vec![],
            notes: vec![],
            steps: vec![
                crate::planner::PlanV2Step {
                    id: "s1".to_string(),
                    op: crate::planner::PlanV2Op::Fetch {
                        root_field: "queryOffshoreWindTurbine".to_string(),
                        fields: vec![
                            "name".to_string(),
                            "shortName".to_string(),
                            "accumulatedDowntime".to_string(),
                        ],
                        first: Some(2000),
                        offset: None,
                        filter: None,
                        order: None,
                    },
                },
                crate::planner::PlanV2Step {
                    id: "s2".to_string(),
                    op: crate::planner::PlanV2Op::FilterRows {
                        source: "s1".to_string(),
                        field: "accumulatedDowntime".to_string(),
                        operator: "gt".to_string(),
                        value: serde_json::json!(400),
                    },
                },
            ],
        };

        assert!(semantic_repair_guard_message(&original, &repaired).is_none());
    }

    #[test]
    fn restore_simple_post_fetch_filters_appends_filter_rows_step() {
        let schema = registry();
        let original = crate::planner::PlanV2 {
            version: Some("v2".to_string()),
            rewrites: vec![],
            notes: vec![],
            steps: vec![crate::planner::PlanV2Step {
                id: "s1".to_string(),
                op: crate::planner::PlanV2Op::Fetch {
                    root_field: "queryOffshoreWindTurbine".to_string(),
                    fields: vec!["name".to_string(), "shortName".to_string()],
                    first: Some(2000),
                    offset: None,
                    filter: Some(serde_json::json!({
                        "accumulatedDowntime": { "gt": 400 }
                    })),
                    order: None,
                },
            }],
        };
        let mut repaired = crate::planner::PlanV2 {
            version: Some("v2".to_string()),
            rewrites: vec![],
            notes: vec![],
            steps: vec![crate::planner::PlanV2Step {
                id: "s1".to_string(),
                op: crate::planner::PlanV2Op::Fetch {
                    root_field: "queryOffshoreWindTurbine".to_string(),
                    fields: vec!["name".to_string(), "shortName".to_string()],
                    first: Some(2000),
                    offset: None,
                    filter: None,
                    order: None,
                },
            }],
        };

        assert!(restore_simple_post_fetch_filters(
            &original,
            &mut repaired,
            &schema
        ));
        assert_eq!(repaired.steps.len(), 2);
        match &repaired.steps[0].op {
            crate::planner::PlanV2Op::Fetch { fields, .. } => {
                assert!(fields.iter().any(|field| field == "accumulatedDowntime"));
            }
            other => panic!("expected fetch step, got {other:?}"),
        }
        match &repaired.steps[1].op {
            crate::planner::PlanV2Op::FilterRows {
                source,
                field,
                operator,
                value,
            } => {
                assert_eq!(source, "s1");
                assert_eq!(field, "accumulatedDowntime");
                assert_eq!(operator, "gt");
                assert_eq!(value, &serde_json::json!("400"));
            }
            other => panic!("expected filter_rows step, got {other:?}"),
        }
    }

    #[test]
    fn deterministic_fallback_guard_blocks_simple_fetch_for_distance_request() {
        let original = crate::planner::PlanV2 {
            version: Some("v2".to_string()),
            rewrites: vec![],
            notes: vec![],
            steps: vec![
                crate::planner::PlanV2Step {
                    id: "s1".to_string(),
                    op: crate::planner::PlanV2Op::Fetch {
                        root_field: "queryVessel".to_string(),
                        fields: vec!["name".to_string()],
                        first: Some(100),
                        offset: None,
                        filter: Some(serde_json::json!({"name": {"eq": "the wagon"}})),
                        order: None,
                    },
                },
                crate::planner::PlanV2Step {
                    id: "s2".to_string(),
                    op: crate::planner::PlanV2Op::Fetch {
                        root_field: "queryOffshoreWindTurbine".to_string(),
                        fields: vec!["shortName".to_string()],
                        first: Some(100),
                        offset: None,
                        filter: Some(serde_json::json!({"shortName": {"eq": "T3"}})),
                        order: None,
                    },
                },
                crate::planner::PlanV2Step {
                    id: "s3".to_string(),
                    op: crate::planner::PlanV2Op::DistanceHaversine {
                        vessels_source: "s1".to_string(),
                        target_source: "s2".to_string(),
                    },
                },
            ],
        };

        let message =
            deterministic_fallback_guard_message(&[&original]).expect("expected fallback guard");
        assert!(message.contains("distance calculation"));
        assert!(message.contains("simple fetch fallback would change the meaning"));
    }
}
