#![allow(clippy::needless_raw_string_hashes, clippy::similar_names)]

use crate::AppState;
use crate::agent::{create_ir_agent, execute_graphql};
use crate::domain_config::FieldRoleSet;
use crate::error::{PipelineError, PipelineResult};
use crate::intermediate_representation::{IRQuery, ir_to_graphql};
use crate::planner::{
    ExecutedArtifact, MultiStepPlan, PlanV2Op, collect_query_scope_literals, scope_guard_message,
};
use crate::progress::{PipelineProgressEvent, ProgressCallback, emit_progress};
use crate::prompts::{QueryRepairPromptContext, build_query_repair_prompt};
use crate::query_repair::{
    extract_backend_invalid_fields, has_unresolved_placeholders, maybe_build_empty_rows_retry,
    maybe_build_error_retry, maybe_rewrite_filter_operator_aliases,
    preserve_identifier_eq_semantics,
};
use crate::schema_registry::SchemaRegistry;
use crate::transformations::{
    LocationFieldHints, RecordFieldHints, RowDisplayRoles, RowsDisplayHints, aggregate_metrics,
    compare_rows, compute_distance_rows, filter_rows, join_on_time_rows, rank_rows,
    render_aggregate_result_summary, render_rows_compact_summary, render_rows_summary,
    render_rows_summary_with_hints, render_trend_summary, row_has_metric_keys,
    summarize_trend_rows, threshold_check_rows,
};
use chrono::Utc;
use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::BTreeSet;

fn executor_op_kind_name(op: &PlanV2Op) -> &'static str {
    match op {
        PlanV2Op::Fetch { .. } => "fetch",
        PlanV2Op::Aggregate { .. } => "aggregate",
        PlanV2Op::Compare { .. } => "compare",
        PlanV2Op::FilterRows { .. } => "filter_rows",
        PlanV2Op::Rank { .. } => "rank",
        PlanV2Op::DistanceHaversine { .. } => "distance_haversine",
        PlanV2Op::JoinOnTime { .. } => "join_on_time",
        PlanV2Op::ThresholdCheck { .. } => "threshold_check",
        PlanV2Op::TrendSummary { .. } => "trend_summary",
    }
}

const DEFAULT_TRANSFORM_PREVIEW_ROWS: usize = 3;
const DEFAULT_FETCH_PREVIEW_ROWS: usize = 20;
const EXPLICIT_FULL_PREVIEW_ROWS: usize = 50;

fn user_requested_full_list(user_message: &str) -> bool {
    let lower = user_message.to_ascii_lowercase();
    [
        "show all",
        "list all",
        "display all",
        "full list",
        "complete list",
        "all results",
        "every result",
        "everything",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn row_display_limit(user_message: &str, row_count: usize, default_limit: usize) -> usize {
    if row_count == 0 {
        return 0;
    }
    let limit = if user_requested_full_list(user_message) {
        EXPLICIT_FULL_PREVIEW_ROWS
    } else {
        default_limit
    };
    row_count.min(limit.max(1))
}

fn row_display_hints_for_fetch(
    schema_registry: &SchemaRegistry,
    root_field: &str,
    rows: &[serde_json::Value],
) -> RowsDisplayHints {
    let parent_roles =
        RowDisplayRoles::from_field_roles(&schema_registry.field_roles_for_root(root_field));
    let mut hints = RowsDisplayHints {
        parent_roles,
        ..RowsDisplayHints::default()
    };
    let Some(parent_type) = schema_registry.query_return_type(root_field) else {
        return hints;
    };

    for row in rows {
        let Some(obj) = row.as_object() else {
            continue;
        };
        for (field, value) in obj {
            if !value.is_array() || hints.relation_roles.contains_key(field) {
                continue;
            }
            let Some(relation_type) = schema_registry.object_field_type(parent_type, field) else {
                continue;
            };
            hints.relation_roles.insert(
                field.clone(),
                RowDisplayRoles::from_field_roles(
                    &schema_registry.field_roles_for_type(relation_type),
                ),
            );
            hints
                .relation_record_types
                .insert(field.clone(), relation_type.to_string());
        }
    }
    hints
}

static ROOT_FIELDS_FROM_QUERY_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\b(query[A-Za-z0-9_]+)\s*(?:\(|\{)").expect("valid root fields from query regex")
});
static QUOTED_PLACEHOLDER_REF_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#""\$\{([A-Za-z][A-Za-z0-9_]*)\.([A-Za-z0-9_.]+)\}""#)
        .expect("valid quoted placeholder ref regex")
});
static BARE_PLACEHOLDER_REF_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"\$\{([A-Za-z][A-Za-z0-9_]*)\.([A-Za-z0-9_.]+)\}"#)
        .expect("valid bare placeholder ref regex")
});

fn extract_root_field_from_query(query: &str) -> Option<String> {
    extract_root_fields_from_query(query).into_iter().next()
}

fn extract_root_fields_from_query(query: &str) -> Vec<String> {
    let mut out = Vec::new();
    for caps in ROOT_FIELDS_FROM_QUERY_RE.captures_iter(query) {
        if let Some(m) = caps.get(1) {
            let s = m.as_str().to_string();
            if !s.eq_ignore_ascii_case("query") && !out.iter().any(|v| v == &s) {
                out.push(s);
            }
        }
    }
    out
}

fn normalized_scope_literals(query: &str) -> BTreeSet<String> {
    collect_query_scope_literals(query)
        .into_iter()
        .map(|literal| literal.trim().to_ascii_lowercase())
        .filter(|literal| !literal.is_empty())
        .collect()
}

fn introduces_new_step_scope_literals(previous_query: &str, repaired_query: &str) -> bool {
    let previous = normalized_scope_literals(previous_query);
    if previous.is_empty() {
        return false;
    }
    let repaired = normalized_scope_literals(repaired_query);
    repaired.iter().any(|literal| !previous.contains(literal))
}

fn is_transport_execution_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("error sending request")
        || lower.contains("connection refused")
        || lower.contains("tcp connect error")
        || lower.contains("dns error")
        || lower.contains("timed out")
        || lower.contains("timeout")
        || lower.contains("channel closed")
}

fn value_at_path(row: &serde_json::Value, path: &str) -> Option<serde_json::Value> {
    let mut cur = row;
    for part in path.split('.') {
        if let Some(next) = cur.get(part) {
            cur = next;
        } else {
            return None;
        }
    }
    Some(cur.clone())
}

fn value_is_empty_relation(value: Option<&serde_json::Value>) -> bool {
    match value {
        // If the key is absent, the relation was likely pruned by schema/query repair
        // before execution. Only explicit null/empty values from the backend prove an
        // empty relation.
        None => false,
        Some(serde_json::Value::Null) => true,
        Some(serde_json::Value::Array(items)) => items.is_empty(),
        Some(serde_json::Value::Object(fields)) => fields.is_empty(),
        Some(_) => false,
    }
}

fn detect_empty_parent_rewrite_relations_for_fetch(
    root_field: &str,
    fields: &[String],
    rows: &[serde_json::Value],
) -> Vec<String> {
    if rows.is_empty() {
        return Vec::new();
    }

    let mut relation_prefixes: Vec<String> = Vec::new();
    for field in fields {
        let Some((prefix, _)) = field.split_once('.') else {
            continue;
        };
        if !relation_prefixes
            .iter()
            .any(|existing: &String| existing.eq_ignore_ascii_case(prefix))
        {
            relation_prefixes.push(prefix.to_string());
        }
    }

    relation_prefixes
        .into_iter()
        .filter(|prefix| {
            rows.iter()
                .all(|row| value_is_empty_relation(row.get(prefix.as_str())))
        })
        .map(|prefix| format!("{root_field}.{prefix} returned no child rows"))
        .collect()
}

fn detect_empty_parent_rewrite_relations(
    plan: &MultiStepPlan,
    datasets: &std::collections::HashMap<String, Vec<serde_json::Value>>,
) -> Vec<String> {
    let mut guards = Vec::new();
    for step in &plan.steps {
        let PlanV2Op::Fetch {
            root_field, fields, ..
        } = &step.op
        else {
            continue;
        };
        let Some(rows) = datasets.get(&step.id) else {
            continue;
        };
        guards.extend(detect_empty_parent_rewrite_relations_for_fetch(
            root_field, fields, rows,
        ));
    }
    guards.sort();
    guards.dedup();
    guards
}

fn value_has_meaningful_root_scalar(value: Option<&serde_json::Value>) -> bool {
    match value {
        Some(serde_json::Value::String(text)) => !text.trim().is_empty(),
        Some(serde_json::Value::Number(_)) | Some(serde_json::Value::Bool(_)) => true,
        _ => false,
    }
}

fn empty_relation_backend_note(missing: &[String]) -> Option<String> {
    let child_row_gaps = missing
        .iter()
        .filter_map(|item| item.strip_suffix(" returned no child rows"))
        .collect::<Vec<_>>();
    if child_row_gaps.is_empty() {
        return None;
    }
    Some(format!(
        "Note: the backend returned no child rows for {}.",
        child_row_gaps.join(", ")
    ))
}

fn fetch_summary_with_empty_relation_note(
    plan: &MultiStepPlan,
    fetch_step_id: &str,
    rows: &[serde_json::Value],
    empty_relation_guards: &[String],
) -> Option<String> {
    let fields = plan.steps.iter().find_map(|step| {
        if step.id != fetch_step_id {
            return None;
        }
        match &step.op {
            PlanV2Op::Fetch { fields, .. } => Some(fields),
            _ => None,
        }
    })?;

    let meaningful_root_scalar_count = fields
        .iter()
        .filter(|field| !field.contains('.'))
        .filter(|field| {
            rows.iter()
                .any(|row| value_has_meaningful_root_scalar(row.get(field.as_str())))
        })
        .count();
    if meaningful_root_scalar_count < 2 {
        return None;
    }

    let sample_limit = rows.len().clamp(1, DEFAULT_FETCH_PREVIEW_ROWS);
    let mut answer = render_rows_summary(rows, sample_limit);
    if let Some(note) = empty_relation_backend_note(empty_relation_guards) {
        answer.push(' ');
        answer.push_str(&note);
    }
    Some(answer)
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub(crate) struct ExecutionEvidence {
    pub(crate) row_count: usize,
    pub(crate) sample_rows: Vec<serde_json::Value>,
    pub(crate) literals: Vec<String>,
    pub(crate) time_values: Vec<i64>,
    pub(crate) field_values: std::collections::HashMap<String, Vec<String>>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct ExecutionGrounding {
    pub(crate) mention: String,
    pub(crate) family_type: String,
    pub(crate) root_field: String,
    pub(crate) matched_field: String,
    pub(crate) matched_value: String,
    pub(crate) stable_key_field: Option<String>,
    pub(crate) stable_key_value: Option<String>,
    pub(crate) display_label: Option<String>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub(crate) enum DeterministicAnswerKind {
    RowList,
    MetricSummary,
    DistanceSummary,
    CompareSummary,
    TrendSummary,
    Diagnostic,
}

#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct DeterministicAnswer {
    pub(crate) text: String,
    pub(crate) kind: DeterministicAnswerKind,
}

impl DeterministicAnswer {
    fn new(text: String, kind: DeterministicAnswerKind) -> Self {
        Self { text, kind }
    }
}

fn primitive_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn exact_filter_bindings(filter: Option<&serde_json::Value>) -> Vec<(String, String)> {
    let Some(filter_obj) = filter.and_then(|value| value.as_object()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (field, clause) in filter_obj {
        let Some(clause_obj) = clause.as_object() else {
            continue;
        };
        let Some(eq_value) = clause_obj.get("eq") else {
            continue;
        };
        let Some(rendered) = primitive_string(eq_value) else {
            continue;
        };
        if rendered.contains("${") {
            continue;
        }
        out.push((field.clone(), rendered));
    }
    out
}

fn row_display_label(
    schema_registry: &SchemaRegistry,
    root_field: &str,
    row: &serde_json::Value,
) -> Option<String> {
    let roles = schema_registry.field_roles_for_root(root_field);
    roles
        .label_fields
        .iter()
        .find_map(|field| row.get(field).and_then(primitive_string))
}

fn row_stable_key(
    schema_registry: &SchemaRegistry,
    root_field: &str,
    matched_field: &str,
    row: &serde_json::Value,
) -> (Option<String>, Option<String>) {
    let roles = schema_registry.field_roles_for_root(root_field);
    if let Some(value) = row.get(matched_field).and_then(primitive_string)
        && (roles.id_fields.iter().any(|f| f == matched_field)
            || roles.entity_key_fields.iter().any(|f| f == matched_field))
    {
        return (Some(matched_field.to_string()), Some(value));
    }
    for field in &roles.id_fields {
        if let Some(value) = row.get(field).and_then(primitive_string) {
            return (Some(field.clone()), Some(value));
        }
    }
    for field in &roles.entity_key_fields {
        if let Some(value) = row.get(field).and_then(primitive_string) {
            return (Some(field.clone()), Some(value));
        }
    }
    (None, None)
}

fn execution_groundings_for_fetch(
    schema_registry: &SchemaRegistry,
    root_field: &str,
    filter: Option<&serde_json::Value>,
    rows: &[serde_json::Value],
) -> Vec<ExecutionGrounding> {
    if rows.len() != 1 {
        return Vec::new();
    }
    let Some(row) = rows.first() else {
        return Vec::new();
    };
    let family_type = schema_registry
        .query_return_type(root_field)
        .unwrap_or(root_field)
        .to_string();
    let display_label = row_display_label(schema_registry, root_field, row);
    exact_filter_bindings(filter)
        .into_iter()
        .filter_map(|(matched_field, mention)| {
            let matched_value = row.get(&matched_field).and_then(primitive_string)?;
            if !matched_value.eq_ignore_ascii_case(&mention) {
                return None;
            }
            let (stable_key_field, stable_key_value) =
                row_stable_key(schema_registry, root_field, &matched_field, row);
            Some(ExecutionGrounding {
                mention,
                family_type: family_type.clone(),
                root_field: root_field.to_string(),
                matched_field,
                matched_value,
                stable_key_field,
                stable_key_value,
                display_label: display_label.clone(),
            })
        })
        .collect()
}

fn parse_time_millis_from_value(value: &serde_json::Value) -> Option<i64> {
    match value {
        serde_json::Value::Number(n) => {
            let raw = n.as_i64().or_else(|| n.as_f64().map(|v| v as i64))?;
            if raw.abs() >= 1_000_000_000_000 {
                Some(raw)
            } else if raw.abs() >= 1_000_000_000 {
                Some(raw * 1000)
            } else {
                None
            }
        }
        serde_json::Value::String(s) => {
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
                return Some(dt.timestamp_millis());
            }
            if let Ok(naive_dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
                return Some(naive_dt.and_utc().timestamp_millis());
            }
            if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
                && let Some(dt) = d.and_hms_opt(0, 0, 0)
            {
                return Some(dt.and_utc().timestamp_millis());
            }
            None
        }
        _ => None,
    }
}

fn collect_evidence_from_value(
    value: &serde_json::Value,
    literals: &mut BTreeSet<String>,
    times: &mut BTreeSet<i64>,
    field_values: &mut std::collections::HashMap<String, BTreeSet<String>>,
    prefix: &str,
) {
    match value {
        serde_json::Value::String(s) => {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                literals.insert(trimmed.to_string());
                if !prefix.is_empty() {
                    field_values
                        .entry(prefix.to_string())
                        .or_default()
                        .insert(trimmed.to_string());
                }
            }
            if let Some(ts) = parse_time_millis_from_value(value) {
                times.insert(ts);
            }
        }
        serde_json::Value::Number(n) => {
            literals.insert(n.to_string());
            if !prefix.is_empty() {
                field_values
                    .entry(prefix.to_string())
                    .or_default()
                    .insert(n.to_string());
            }
            if let Some(ts) = parse_time_millis_from_value(value) {
                times.insert(ts);
            }
        }
        serde_json::Value::Bool(b) => {
            literals.insert(b.to_string());
            if !prefix.is_empty() {
                field_values
                    .entry(prefix.to_string())
                    .or_default()
                    .insert(b.to_string());
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_evidence_from_value(item, literals, times, field_values, prefix);
            }
        }
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                let next_prefix = if prefix.is_empty() {
                    key.to_string()
                } else {
                    format!("{prefix}.{key}")
                };
                collect_evidence_from_value(value, literals, times, field_values, &next_prefix);
            }
        }
        _ => {}
    }
}

fn build_execution_evidence(rows: &[serde_json::Value]) -> ExecutionEvidence {
    let mut literals = BTreeSet::new();
    let mut time_values = BTreeSet::new();
    let mut field_values: std::collections::HashMap<String, BTreeSet<String>> =
        std::collections::HashMap::new();
    for row in rows.iter() {
        collect_evidence_from_value(row, &mut literals, &mut time_values, &mut field_values, "");
    }
    let field_values = field_values
        .into_iter()
        .map(|(k, v)| (k, v.into_iter().collect()))
        .collect::<std::collections::HashMap<_, _>>();
    ExecutionEvidence {
        row_count: rows.len(),
        sample_rows: rows.iter().take(3).cloned().collect(),
        literals: literals.into_iter().collect(),
        time_values: time_values.into_iter().collect(),
        field_values,
    }
}

fn rows_from_lookup_response(body: &serde_json::Value, root_field: &str) -> Vec<serde_json::Value> {
    let Some(root_value) = body.get("data").and_then(|data| data.get(root_field)) else {
        return Vec::new();
    };
    if let Some(items) = root_value.as_array() {
        return items.to_vec();
    }
    if root_value.is_object() {
        return vec![root_value.clone()];
    }
    Vec::new()
}

fn grouped_relation_field_name(group_field: &str) -> Option<String> {
    let field = group_field
        .strip_suffix("Uid")
        .or_else(|| group_field.strip_suffix("Id"))?;
    (!field.is_empty()).then_some(field.to_string())
}

fn grouped_target_root_for_field(
    schema_registry: &SchemaRegistry,
    source_root: &str,
    group_field: &str,
) -> Option<String> {
    let source_type = schema_registry.query_return_type(source_root)?;
    let relation_field = grouped_relation_field_name(group_field)?;
    let target_type = schema_registry.object_field_type(source_type, &relation_field)?;
    let target_root = format!("query{target_type}");
    schema_registry
        .query_return_type(&target_root)
        .is_some()
        .then_some(target_root)
}

fn target_label_fields(schema_registry: &SchemaRegistry, target_root: &str) -> Vec<String> {
    let roles = schema_registry.field_roles_for_root(target_root);
    let Some(target_type) = schema_registry.query_return_type(target_root) else {
        return Vec::new();
    };
    let mut fields = Vec::new();
    for field in &roles.label_fields {
        let Some(field_type) = schema_registry.object_field_type(target_type, field) else {
            continue;
        };
        if schema_registry.object_field_names(field_type).is_some() {
            continue;
        }
        if !fields.iter().any(|existing| existing == field) {
            fields.push(field.clone());
        }
    }
    fields
}

fn string_field(row: &serde_json::Value, field: &str) -> Option<String> {
    row.get(field).and_then(|value| match value {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    })
}

fn scalar_field_value(row: &serde_json::Value, field: &str) -> Option<serde_json::Value> {
    row.get(field).and_then(|value| match value {
        serde_json::Value::String(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::Bool(_) => Some(value.clone()),
        _ => None,
    })
}

fn carry_single_source_identity_fields(
    rows: &mut [serde_json::Value],
    source_rows: &[serde_json::Value],
    roles: &FieldRoleSet,
) {
    let [source_row] = source_rows else {
        return;
    };
    if rows.is_empty() {
        return;
    }
    let candidate_fields = roles
        .label_fields
        .iter()
        .chain(roles.entity_key_fields.iter())
        .chain(roles.id_fields.iter());
    for field in candidate_fields {
        let Some(value) = scalar_field_value(source_row, field) else {
            continue;
        };
        for row in rows.iter_mut() {
            let Some(obj) = row.as_object_mut() else {
                continue;
            };
            obj.entry(field.clone()).or_insert_with(|| value.clone());
        }
    }
}

fn preferred_label_for_row(
    row: &serde_json::Value,
    schema_registry: &SchemaRegistry,
    target_root: &str,
) -> Option<String> {
    for field in target_label_fields(schema_registry, target_root) {
        if let Some(value) = string_field(row, &field) {
            return Some(value);
        }
    }
    None
}

fn build_in_filter(
    schema_registry: &SchemaRegistry,
    root_field: &str,
    field: &str,
    values: &[String],
) -> Option<serde_json::Value> {
    let type_ref = schema_registry.filter_field_type_ref(root_field, field)?;
    let op_fields = schema_registry.input_field_names(&type_ref.name)?;
    if op_fields.iter().any(|op| op.eq_ignore_ascii_case("in")) {
        return Some(serde_json::json!({
            field: {
                "in": values
            }
        }));
    }
    if values.len() == 1 && op_fields.iter().any(|op| op.eq_ignore_ascii_case("eq")) {
        return Some(serde_json::json!({
            field: {
                "eq": values[0]
            }
        }));
    }
    None
}

async fn grouped_label_lookup_map(
    state: &AppState,
    schema_registry: &SchemaRegistry,
    target_root: &str,
    values: &[String],
    debug_logs: &mut Vec<String>,
) -> Option<std::collections::HashMap<String, String>> {
    if values.is_empty() || state.config.graph.graph_endpoint.trim().is_empty() {
        if values.is_empty() {
            debug_logs.push(format!(
                "[LABEL_HYDRATION] skipped target_root=`{target_root}` because there were no grouped values"
            ));
        } else {
            debug_logs.push(format!(
                "[LABEL_HYDRATION] skipped target_root=`{target_root}` because no GraphQL endpoint is configured"
            ));
        }
        return None;
    }
    let roles = schema_registry.field_roles_for_root(target_root);
    let filter_fields = schema_registry.root_filter_fields(target_root);
    let mut candidate_key_fields = Vec::new();
    for field in roles.id_fields.iter().chain(roles.entity_key_fields.iter()) {
        if filter_fields
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(field))
            && !candidate_key_fields
                .iter()
                .any(|existing: &String| existing == field)
        {
            candidate_key_fields.push(field.clone());
        }
    }
    if candidate_key_fields.is_empty() {
        debug_logs.push(format!(
            "[LABEL_HYDRATION] no candidate key fields for target_root=`{target_root}`"
        ));
        return None;
    }
    debug_logs.push(format!(
        "[LABEL_HYDRATION] target_root=`{target_root}` trying key_fields=[{}] for {} grouped value(s)",
        candidate_key_fields.join(", "),
        values.len()
    ));

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

    let mut best_map = None;
    let mut best_score = 0usize;
    for key_field in candidate_key_fields {
        let Some(filter) = build_in_filter(schema_registry, target_root, &key_field, values) else {
            debug_logs.push(format!(
                "[LABEL_HYDRATION] target_root=`{target_root}` key_field=`{key_field}` has no usable eq/in filter shape"
            ));
            continue;
        };
        let mut fields = vec![key_field.clone()];
        for field in target_label_fields(schema_registry, target_root) {
            if !fields.iter().any(|existing| existing == &field) {
                fields.push(field);
            }
        }
        let Some(query) = ir_to_graphql(&IRQuery {
            root_field: target_root.to_string(),
            fields,
            first: Some(values.len() as i64),
            offset: None,
            filter: Some(filter),
            order: None,
        }) else {
            debug_logs.push(format!(
                "[LABEL_HYDRATION] target_root=`{target_root}` key_field=`{key_field}` could not be compiled to GraphQL"
            ));
            continue;
        };

        debug_logs.push(format!(
            "[LABEL_HYDRATION] target_root=`{target_root}` key_field=`{key_field}` executing lookup for values={:?}",
            values
        ));

        let Ok(response) = execute_graphql(
            &state.client,
            &state.config.graph.graph_endpoint,
            bearer_token,
            api_key_header,
            api_key,
            &query,
            &serde_json::json!({}),
        )
        .await
        else {
            debug_logs.push(format!(
                "[LABEL_HYDRATION] target_root=`{target_root}` key_field=`{key_field}` request failed"
            ));
            continue;
        };
        if let Some(errors) = response.get("errors").and_then(|value| value.as_array())
            && !errors.is_empty()
        {
            debug_logs.push(format!(
                "[LABEL_HYDRATION] target_root=`{target_root}` key_field=`{key_field}` GraphQL errors: {}",
                serde_json::to_string(errors).unwrap_or_else(|_| "[]".to_string())
            ));
            continue;
        }

        let response_rows = rows_from_lookup_response(&response, target_root);
        debug_logs.push(format!(
            "[LABEL_HYDRATION] target_root=`{target_root}` key_field=`{key_field}` returned {} row(s)",
            response_rows.len()
        ));

        let mut label_map = std::collections::HashMap::new();
        for row in response_rows {
            let Some(key_value) = string_field(&row, &key_field) else {
                continue;
            };
            let Some(label) = preferred_label_for_row(&row, schema_registry, target_root) else {
                continue;
            };
            label_map.insert(key_value, label);
        }
        let score = values
            .iter()
            .filter(|value| label_map.contains_key(*value))
            .count();
        debug_logs.push(format!(
            "[LABEL_HYDRATION] target_root=`{target_root}` key_field=`{key_field}` matched_labels={score}/{}",
            values.len()
        ));
        if score > best_score {
            best_score = score;
            best_map = Some(label_map);
        }
    }

    if best_score == 0 {
        debug_logs.push(format!(
            "[LABEL_HYDRATION] target_root=`{target_root}` produced no label matches for grouped values"
        ));
    }

    best_map.filter(|map| !map.is_empty())
}

async fn hydrate_grouped_foreign_key_labels(
    state: &AppState,
    schema_registry: &SchemaRegistry,
    source_root: &str,
    group_by: &[String],
    rows: &mut [serde_json::Value],
    debug_logs: &mut Vec<String>,
) {
    for group_field in group_by {
        let values = rows
            .iter()
            .filter_map(|row| row.get(group_field).and_then(serde_json::Value::as_str))
            .map(str::to_string)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        if values.is_empty() {
            debug_logs.push(format!(
                "[LABEL_HYDRATION] group_field=`{group_field}` had no non-empty grouped values"
            ));
            continue;
        }
        let Some(target_root) =
            grouped_target_root_for_field(schema_registry, source_root, group_field)
        else {
            debug_logs.push(format!(
                "[LABEL_HYDRATION] group_field=`{group_field}` could not derive a target root from source_root=`{source_root}`"
            ));
            continue;
        };
        let Some(label_map) =
            grouped_label_lookup_map(state, schema_registry, &target_root, &values, debug_logs)
                .await
        else {
            continue;
        };

        let label_field = format!("{group_field}_label");
        let mut hydrated = 0usize;
        for row in rows.iter_mut() {
            let Some(raw_value) = row.get(group_field).and_then(serde_json::Value::as_str) else {
                continue;
            };
            let Some(label) = label_map.get(raw_value) else {
                continue;
            };
            if let Some(obj) = row.as_object_mut() {
                obj.insert(label_field.clone(), serde_json::json!(label));
                hydrated += 1;
            }
        }
        if hydrated > 0 {
            debug_logs.push(format!(
                "[LABEL_HYDRATION] grouped field `{group_field}` hydrated via `{target_root}` for {hydrated} row(s)"
            ));
        }
    }
}

fn select_evidence_rows(
    datasets: &std::collections::HashMap<String, Vec<serde_json::Value>>,
    fetched_step_ids: &[String],
    last_result: Option<&Vec<serde_json::Value>>,
) -> Vec<serde_json::Value> {
    if let Some(rows) = last_result
        && rows.len() == 1
    {
        let left_source = rows[0].get("left_source").and_then(|v| v.as_str());
        let right_source = rows[0].get("right_source").and_then(|v| v.as_str());
        if let (Some(left_source), Some(right_source)) = (left_source, right_source) {
            let mut merged = Vec::new();
            if let Some(left_rows) = datasets.get(left_source) {
                merged.extend(left_rows.iter().cloned());
            }
            if let Some(right_rows) = datasets.get(right_source) {
                merged.extend(right_rows.iter().cloned());
            }
            if !merged.is_empty() {
                return merged;
            }
        }
    }
    if let Some(rows) = last_result {
        return rows.clone();
    }
    if let Some(last_fetch) = fetched_step_ids.last()
        && let Some(rows) = datasets.get(last_fetch)
    {
        return rows.clone();
    }
    Vec::new()
}

pub(crate) async fn execute_multistep_plan_with_progress(
    state: &AppState,
    schema_registry: &SchemaRegistry,
    model_name: &str,
    user_message: &str,
    plan: &MultiStepPlan,
    progress: Option<ProgressCallback<'_>>,
) -> PipelineResult<(
    DeterministicAnswer,
    Vec<ExecutedArtifact>,
    ExecutionEvidence,
    Vec<ExecutionGrounding>,
)> {
    async fn run_graphql_query(
        state: &AppState,
        schema_registry: &SchemaRegistry,
        model_name: &str,
        user_message: &str,
        query: &str,
        datasets: &std::collections::HashMap<String, Vec<serde_json::Value>>,
        expected_root: Option<&str>,
    ) -> PipelineResult<(serde_json::Value, String, Vec<String>)> {
        fn one_line_excerpt(s: &str, max_chars: usize) -> String {
            let compact = s.replace(['\n', '\r'], " ");
            if compact.len() <= max_chars {
                compact
            } else {
                format!("{}...", &compact[..max_chars])
            }
        }
        fn extract_graphql_query_from_text(text: &str) -> Option<String> {
            fn normalize_graphql_candidate(text: &str) -> String {
                let mut s = text.trim().to_string();
                if let Ok(serde_json::Value::String(inner)) =
                    serde_json::from_str::<serde_json::Value>(&s)
                {
                    s = inner;
                }
                if s.contains("\\r\\n")
                    || s.contains("\\n")
                    || s.contains("\\t")
                    || s.contains("\\\"")
                {
                    s = s
                        .replace("\\r\\n", "\n")
                        .replace("\\n", "\n")
                        .replace("\\t", "\t")
                        .replace("\\\"", "\"");
                }
                s.trim().to_string()
            }

            fn query_from_json_wrapper(text: &str) -> Option<String> {
                let v = serde_json::from_str::<serde_json::Value>(text).ok()?;
                match v {
                    serde_json::Value::Object(map) => {
                        if let Some(q) = map.get("query").and_then(|v| v.as_str()) {
                            return Some(normalize_graphql_candidate(q));
                        }
                        if let Some(q) = map
                            .get("data")
                            .and_then(|v| v.get("query"))
                            .and_then(|v| v.as_str())
                        {
                            return Some(normalize_graphql_candidate(q));
                        }
                        None
                    }
                    _ => None,
                }
            }

            let trimmed = text.trim();
            if let Some(unwrapped) = query_from_json_wrapper(trimmed) {
                return Some(unwrapped);
            }
            if trimmed.starts_with("query ")
                || trimmed.starts_with("query\n")
                || trimmed.starts_with("mutation ")
                || trimmed.starts_with("mutation\n")
                || trimmed.starts_with("subscription ")
                || trimmed.starts_with("subscription\n")
                || trimmed.starts_with('{')
            {
                return Some(normalize_graphql_candidate(trimmed));
            }
            if let Some(start) = trimmed.find("```graphql") {
                let rest = &trimmed[start + 10..];
                if let Some(end) = rest.find("```") {
                    let q = rest[..end].trim();
                    if let Some(unwrapped) = query_from_json_wrapper(q) {
                        return Some(unwrapped);
                    }
                    if q.starts_with("query ")
                        || q.starts_with("query\n")
                        || q.starts_with("mutation ")
                        || q.starts_with("mutation\n")
                        || q.starts_with("subscription ")
                        || q.starts_with("subscription\n")
                        || q.starts_with('{')
                    {
                        return Some(normalize_graphql_candidate(q));
                    }
                }
            }
            if let Some(start) = trimmed.find("query ") {
                let q = &trimmed[start..];
                if let Some(end) = q.rfind('}') {
                    return Some(normalize_graphql_candidate(&q[..=end]));
                }
                return Some(normalize_graphql_candidate(q));
            }
            None
        }

        fn ensure_graphql_document(query: &str) -> String {
            let trimmed = query.trim();
            if trimmed.is_empty() {
                return String::new();
            }

            if let Some(extracted) = extract_graphql_query_from_text(trimmed) {
                return extracted.trim().to_string();
            }

            let lower = trimmed.to_lowercase();
            if trimmed.starts_with('{')
                || lower.starts_with("query ")
                || lower.starts_with("query\n")
                || lower.starts_with("mutation ")
                || lower.starts_with("mutation\n")
                || lower.starts_with("subscription ")
                || lower.starts_with("subscription\n")
                || lower.starts_with("fragment ")
            {
                return trimmed.to_string();
            }

            format!("query AutoIR {{\n  {trimmed}\n}}")
        }

        fn sorted_field_names(fields: &std::collections::HashSet<String>, limit: usize) -> String {
            let mut names = fields.iter().cloned().collect::<Vec<_>>();
            names.sort();
            if names.len() > limit {
                let total = names.len();
                names.truncate(limit);
                format!("{} ... ({} total)", names.join(", "), total)
            } else {
                names.join(", ")
            }
        }

        fn extract_type_names_from_error(error_text: &str) -> Vec<String> {
            let mut out = Vec::new();
            let patterns = [
                r"(?:type|input)\s+'([A-Za-z_][A-Za-z0-9_]*)'",
                r#"(?:type|input)\s+"([A-Za-z_][A-Za-z0-9_]*)""#,
                r#"(?:type|input)\s+\\\"([A-Za-z_][A-Za-z0-9_]*)\\\""#,
                r#"on type\s+"([A-Za-z_][A-Za-z0-9_]*)""#,
                r#"on type\s+\\\"([A-Za-z_][A-Za-z0-9_]*)\\\""#,
                r#"by type\s+"([A-Za-z_][A-Za-z0-9_]*)""#,
                r#"by type\s+\\\"([A-Za-z_][A-Za-z0-9_]*)\\\""#,
            ];
            for pat in patterns {
                if let Ok(re) = regex::Regex::new(pat) {
                    for caps in re.captures_iter(error_text) {
                        if let Some(m) = caps.get(1) {
                            let name = m.as_str().to_string();
                            if !out.iter().any(|v: &String| v == &name) {
                                out.push(name);
                            }
                            if out.len() >= 6 {
                                return out;
                            }
                        }
                    }
                }
            }
            out
        }

        fn repair_schema_context(
            schema_registry: &SchemaRegistry,
            broken_query: &str,
            error_text: &str,
        ) -> String {
            let mut lines = Vec::new();

            if let Some(root) = extract_root_field_from_query(broken_query) {
                if let Some(ret_type) = schema_registry.query_return_type(&root) {
                    lines.push(format!("Root `{root}` returns `{ret_type}`."));
                    if let Some(obj_fields) = schema_registry.object_field_names(ret_type) {
                        lines.push(format!(
                            "Valid fields on `{ret_type}`: {}",
                            sorted_field_names(obj_fields, 40)
                        ));
                    }
                }
                if let Some(filter_input) = schema_registry.query_filter_input(&root) {
                    lines.push(format!("Filter input for `{root}`: `{filter_input}`."));
                    if let Some(filter_fields) = schema_registry.input_field_names(filter_input) {
                        lines.push(format!(
                            "Valid filter fields on `{filter_input}`: {}",
                            sorted_field_names(filter_fields, 40)
                        ));
                    }
                }
                if let Some(order_input) = schema_registry.query_order_input(&root) {
                    lines.push(format!("Order input for `{root}`: `{order_input}`."));
                    if let Some(order_fields) = schema_registry.input_field_names(order_input) {
                        lines.push(format!(
                            "Valid order fields on `{order_input}`: {}",
                            sorted_field_names(order_fields, 40)
                        ));
                    }
                }
            }

            for ty in extract_type_names_from_error(error_text) {
                if let Some(obj_fields) = schema_registry.object_field_names(&ty) {
                    lines.push(format!(
                        "Object `{ty}` valid fields: {}",
                        sorted_field_names(obj_fields, 30)
                    ));
                    let mut nested = obj_fields.iter().cloned().collect::<Vec<_>>();
                    nested.sort();
                    for field_name in nested.into_iter().take(12) {
                        if let Some(child_type) =
                            schema_registry.object_field_type(&ty, &field_name)
                            && let Some(child_fields) =
                                schema_registry.object_field_names(child_type)
                        {
                            lines.push(format!(
                                "`{ty}.{field_name}` type `{child_type}` fields: {}",
                                sorted_field_names(child_fields, 20)
                            ));
                        }
                    }
                } else if let Some(input_fields) = schema_registry.input_field_names(&ty) {
                    lines.push(format!(
                        "Input `{ty}` valid fields: {}",
                        sorted_field_names(input_fields, 30)
                    ));
                }
            }

            let location_hints = &schema_registry.domain_config().location_fields;
            if !(location_hints.latitude_fields.is_empty()
                && location_hints.longitude_fields.is_empty()
                && location_hints.geo_object_fields.is_empty())
            {
                let mut lat = location_hints.latitude_fields.clone();
                let mut lon = location_hints.longitude_fields.clone();
                let mut geo = location_hints.geo_object_fields.clone();
                lat.sort();
                lon.sort();
                geo.sort();
                lines.push(format!(
                    "Schema-derived location hints: latitude fields [{}], longitude fields [{}], geo object fields [{}].",
                    lat.join(", "),
                    lon.join(", "),
                    geo.join(", ")
                ));
            }

            if lines.is_empty() {
                "No extra introspection hints.".to_string()
            } else {
                lines.join("\n")
            }
        }

        fn graphql_literal(value: &serde_json::Value) -> String {
            match value {
                serde_json::Value::String(s) => {
                    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
                }
                serde_json::Value::Number(_)
                | serde_json::Value::Bool(_)
                | serde_json::Value::Null => value.to_string(),
                _ => value.to_string(),
            }
        }

        fn lookup_placeholder_value(
            datasets: &std::collections::HashMap<String, Vec<serde_json::Value>>,
            step_id: &str,
            field_path: &str,
        ) -> Option<serde_json::Value> {
            datasets.get(step_id).and_then(|rows| {
                rows.iter().find_map(|row| {
                    let v = value_at_path(row, field_path)?;
                    if v.is_null() { None } else { Some(v) }
                })
            })
        }

        fn resolve_query_placeholders(
            query: &str,
            datasets: &std::collections::HashMap<String, Vec<serde_json::Value>>,
        ) -> String {
            let mut out = query.to_string();
            out = QUOTED_PLACEHOLDER_REF_RE
                .replace_all(&out, |caps: &regex::Captures<'_>| {
                    let step_id = caps.get(1).map(|m| m.as_str()).unwrap_or("");
                    let field_path = caps.get(2).map(|m| m.as_str()).unwrap_or("");
                    lookup_placeholder_value(datasets, step_id, field_path)
                        .map(|v| graphql_literal(&v))
                        .unwrap_or_else(|| {
                            caps.get(0).map(|m| m.as_str()).unwrap_or("").to_string()
                        })
                })
                .to_string();

            BARE_PLACEHOLDER_REF_RE
                .replace_all(&out, |caps: &regex::Captures<'_>| {
                    let step_id = caps.get(1).map(|m| m.as_str()).unwrap_or("");
                    let field_path = caps.get(2).map(|m| m.as_str()).unwrap_or("");
                    lookup_placeholder_value(datasets, step_id, field_path)
                        .map(|v| graphql_literal(&v))
                        .unwrap_or_else(|| {
                            caps.get(0).map(|m| m.as_str()).unwrap_or("").to_string()
                        })
                })
                .to_string()
        }

        fn graphql_error_messages(body: &serde_json::Value) -> Option<String> {
            let errors = body.get("errors")?.as_array()?;
            if errors.is_empty() {
                return None;
            }
            let joined = errors
                .iter()
                .map(|err| {
                    err.get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("Unknown GraphQL error")
                        .to_string()
                })
                .collect::<Vec<_>>()
                .join("; ");
            if joined.trim().is_empty() {
                None
            } else {
                Some(joined)
            }
        }

        fn dataset_context(
            datasets: &std::collections::HashMap<String, Vec<serde_json::Value>>,
        ) -> String {
            if datasets.is_empty() {
                return "none".to_string();
            }
            let mut keys = datasets.keys().cloned().collect::<Vec<_>>();
            keys.sort();
            keys.into_iter()
                .take(5)
                .map(|k| {
                    let rows = datasets.get(&k).cloned().unwrap_or_default();
                    let sample = rows.first().cloned().unwrap_or(serde_json::json!(null));
                    format!("{k}: {} row(s), sample={sample}", rows.len())
                })
                .collect::<Vec<_>>()
                .join(" | ")
        }

        #[allow(clippy::too_many_arguments)]
        async fn repair_multistep_query_with_llm(
            state: &AppState,
            schema_registry: &SchemaRegistry,
            model_name: &str,
            user_message: &str,
            broken_query: &str,
            error_text: &str,
            datasets: &std::collections::HashMap<String, Vec<serde_json::Value>>,
            expected_root: Option<&str>,
        ) -> Option<String> {
            let schema_snippet = schema_registry.search(user_message);
            let root_fields = schema_registry
                .root_fields()
                .into_iter()
                .filter(|f| f.starts_with("query"))
                .collect::<Vec<_>>()
                .join(", ");
            let dataset_ctx = dataset_context(datasets);
            let repair_ctx = repair_schema_context(schema_registry, broken_query, error_text);
            let forbidden_fields = extract_backend_invalid_fields(error_text);
            let forbidden_fields_text = if forbidden_fields.is_empty() {
                "none".to_string()
            } else {
                forbidden_fields.join(", ")
            };
            let step_constraints = crate::planner::collect_query_scope_literals(broken_query);
            let step_constraints_text = if step_constraints.is_empty() {
                "none extracted from this step".to_string()
            } else {
                step_constraints.join(", ")
            };
            let today_utc = Utc::now().date_naive().to_string();
            let step_scope = if let Some(root) = expected_root {
                format!(
                    "This step must query ONLY root field `{root}` (single root for this step)."
                )
            } else {
                "This step should stay single-root unless the original step is explicitly multi-root."
                    .to_string()
            };
            let prompt = build_query_repair_prompt(&QueryRepairPromptContext {
                forbidden_fields_text: &forbidden_fields_text,
                today_utc: &today_utc,
                step_scope: &step_scope,
                step_constraints_text: &step_constraints_text,
                user_message,
                error_text,
                root_fields: &root_fields,
                dataset_ctx: &dataset_ctx,
                repair_ctx: &repair_ctx,
                schema_snippet: &schema_snippet,
                broken_query,
            });
            let agent = if model_name.is_empty() || model_name == state.config.model {
                state.cached_ir_agent.clone()
            } else {
                create_ir_agent(&state.config, model_name).await.ok()?
            };
            let fixed = agent.prompt_text(&prompt).await.ok()?;
            extract_graphql_query_from_text(&fixed)
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
        let mut candidate = resolve_query_placeholders(&ensure_graphql_document(query), datasets);
        if let Some(adapted) = maybe_rewrite_filter_operator_aliases(&candidate) {
            candidate = adapted;
        }
        let mut last_err = String::new();
        let mut last_query = candidate.clone();
        let mut repair_trace = Vec::new();
        let mut transport_error = false;
        repair_trace.push(format!(
            "[REPAIR] start expected_root={}",
            expected_root.unwrap_or("(none)")
        ));
        repair_trace.push(format!(
            "[REPAIR] initial_query={}",
            one_line_excerpt(&candidate, 360)
        ));

        const MAX_REPAIR_ATTEMPTS: usize = 4;
        for attempt in 0..=MAX_REPAIR_ATTEMPTS {
            repair_trace.push(format!("[REPAIR] attempt {attempt}"));
            let mut deterministic_retry: Option<String> = None;
            last_query = candidate.clone();
            if has_unresolved_placeholders(&candidate) {
                last_err =
                    "Multi-step query validation failed: unresolved placeholder(s) remain in query."
                        .to_string();
                repair_trace.push("[REPAIR] unresolved placeholders detected".to_string());
            } else if let Some(expected) = expected_root {
                let roots = extract_root_fields_from_query(&candidate);
                if roots.is_empty() {
                    last_err = format!(
                        "Multi-step query validation failed: no root field detected; expected `{expected}`."
                    );
                    repair_trace.push("[REPAIR] no root field detected".to_string());
                } else if roots.len() > 1 {
                    last_err = format!(
                        "Multi-step query validation failed: step must be single-root, but query has roots [{}].",
                        roots.join(", ")
                    );
                    repair_trace.push(format!(
                        "[REPAIR] invalid multi-root query roots=[{}]",
                        roots.join(", ")
                    ));
                } else if let Err(e) = schema_registry.validate_query(&candidate) {
                    last_err = format!("Multi-step query validation failed: {e}");
                    repair_trace.push(format!(
                        "[REPAIR] schema validation error={}",
                        one_line_excerpt(&last_err, 260)
                    ));
                    if let Some((suffix, adapted)) = maybe_build_error_retry(
                        schema_registry,
                        &candidate,
                        &last_err,
                        attempt,
                        MAX_REPAIR_ATTEMPTS,
                    ) {
                        last_err.push_str(&suffix);
                        deterministic_retry = Some(adapted);
                        repair_trace.push(format!(
                            "[REPAIR] deterministic_retry={}",
                            one_line_excerpt(suffix.trim(), 200)
                        ));
                    }
                } else {
                    match execute_graphql(
                        &state.client,
                        &state.config.graph.graph_endpoint,
                        bearer_token,
                        api_key_header,
                        api_key,
                        &candidate,
                        &serde_json::json!({}),
                    )
                    .await
                    {
                        Ok(body) => {
                            let mut had_graphql_errors = false;
                            if let Some(graphql_errors) = graphql_error_messages(&body) {
                                had_graphql_errors = true;
                                last_err = format!("GraphQL execution errors: {graphql_errors}");
                                repair_trace.push(format!(
                                    "[REPAIR] graphql execution errors={}",
                                    one_line_excerpt(&last_err, 260)
                                ));
                                if let Some((suffix, adapted)) = maybe_build_error_retry(
                                    schema_registry,
                                    &candidate,
                                    &last_err,
                                    attempt,
                                    MAX_REPAIR_ATTEMPTS,
                                ) {
                                    last_err.push_str(&suffix);
                                    deterministic_retry = Some(adapted);
                                    repair_trace.push(format!(
                                        "[REPAIR] deterministic_retry={}",
                                        one_line_excerpt(suffix.trim(), 200)
                                    ));
                                }
                            }
                            let mut should_retry_empty = had_graphql_errors;
                            if !had_graphql_errors {
                                let current_root = roots
                                    .first()
                                    .cloned()
                                    .unwrap_or_else(|| expected.to_string());
                                if let Some(rows) = body
                                    .get("data")
                                    .and_then(|d| d.get(&current_root))
                                    .and_then(|v| v.as_array())
                                    && let Some((err, retry)) = maybe_build_empty_rows_retry(
                                        schema_registry,
                                        &candidate,
                                        attempt,
                                        MAX_REPAIR_ATTEMPTS,
                                        &current_root,
                                        rows,
                                    )
                                {
                                    last_err = err;
                                    deterministic_retry = retry;
                                    should_retry_empty = true;
                                    repair_trace.push(format!(
                                        "[REPAIR] empty-rows retry on root={current_root}: {}",
                                        one_line_excerpt(&last_err, 220)
                                    ));
                                }
                            }
                            if !should_retry_empty {
                                repair_trace.push("[REPAIR] success".to_string());
                                return Ok((body, candidate, repair_trace));
                            }
                        }
                        Err(e) => {
                            last_err = e.to_string();
                            repair_trace.push(format!(
                                "[REPAIR] execution error={}",
                                one_line_excerpt(&last_err, 260)
                            ));
                            if is_transport_execution_error(&last_err) {
                                transport_error = true;
                            }
                            if let Some((suffix, adapted)) = maybe_build_error_retry(
                                schema_registry,
                                &candidate,
                                &last_err,
                                attempt,
                                MAX_REPAIR_ATTEMPTS,
                            ) {
                                last_err.push_str(&suffix);
                                deterministic_retry = Some(adapted);
                                repair_trace.push(format!(
                                    "[REPAIR] deterministic_retry={}",
                                    one_line_excerpt(suffix.trim(), 200)
                                ));
                            }
                        }
                    }
                }
            } else if let Err(e) = schema_registry.validate_query(&candidate) {
                last_err = format!("Multi-step query validation failed: {e}");
                repair_trace.push(format!(
                    "[REPAIR] schema validation error={}",
                    one_line_excerpt(&last_err, 260)
                ));
                if let Some((suffix, adapted)) = maybe_build_error_retry(
                    schema_registry,
                    &candidate,
                    &last_err,
                    attempt,
                    MAX_REPAIR_ATTEMPTS,
                ) {
                    last_err.push_str(&suffix);
                    deterministic_retry = Some(adapted);
                    repair_trace.push(format!(
                        "[REPAIR] deterministic_retry={}",
                        one_line_excerpt(suffix.trim(), 200)
                    ));
                }
            } else {
                match execute_graphql(
                    &state.client,
                    &state.config.graph.graph_endpoint,
                    bearer_token,
                    api_key_header,
                    api_key,
                    &candidate,
                    &serde_json::json!({}),
                )
                .await
                {
                    Ok(body) => {
                        let mut had_graphql_errors = false;
                        if let Some(graphql_errors) = graphql_error_messages(&body) {
                            had_graphql_errors = true;
                            last_err = format!("GraphQL execution errors: {graphql_errors}");
                            repair_trace.push(format!(
                                "[REPAIR] graphql execution errors={}",
                                one_line_excerpt(&last_err, 260)
                            ));
                            if let Some((suffix, adapted)) = maybe_build_error_retry(
                                schema_registry,
                                &candidate,
                                &last_err,
                                attempt,
                                MAX_REPAIR_ATTEMPTS,
                            ) {
                                last_err.push_str(&suffix);
                                deterministic_retry = Some(adapted);
                                repair_trace.push(format!(
                                    "[REPAIR] deterministic_retry={}",
                                    one_line_excerpt(suffix.trim(), 200)
                                ));
                            }
                        }
                        let mut should_retry_empty = had_graphql_errors;
                        if !had_graphql_errors
                            && let Some(root_field) = extract_root_field_from_query(&candidate)
                            && let Some(rows) = body
                                .get("data")
                                .and_then(|d| d.get(&root_field))
                                .and_then(|v| v.as_array())
                            && let Some((err, retry)) = maybe_build_empty_rows_retry(
                                schema_registry,
                                &candidate,
                                attempt,
                                MAX_REPAIR_ATTEMPTS,
                                &root_field,
                                rows,
                            )
                        {
                            last_err = err;
                            deterministic_retry = retry;
                            should_retry_empty = true;
                            repair_trace.push(format!(
                                "[REPAIR] empty-rows retry on root={root_field}: {}",
                                one_line_excerpt(&last_err, 220)
                            ));
                        }
                        if !should_retry_empty {
                            repair_trace.push("[REPAIR] success".to_string());
                            return Ok((body, candidate, repair_trace));
                        }
                    }
                    Err(e) => {
                        last_err = e.to_string();
                        repair_trace.push(format!(
                            "[REPAIR] execution error={}",
                            one_line_excerpt(&last_err, 260)
                        ));
                        if is_transport_execution_error(&last_err) {
                            transport_error = true;
                        }
                        if let Some((suffix, adapted)) = maybe_build_error_retry(
                            schema_registry,
                            &candidate,
                            &last_err,
                            attempt,
                            MAX_REPAIR_ATTEMPTS,
                        ) {
                            last_err.push_str(&suffix);
                            deterministic_retry = Some(adapted);
                            repair_trace.push(format!(
                                "[REPAIR] deterministic_retry={}",
                                one_line_excerpt(suffix.trim(), 200)
                            ));
                        }
                    }
                }
            }

            if let Some(next_query) = deterministic_retry {
                repair_trace.push(format!(
                    "[REPAIR] next_query(deterministic)={}",
                    one_line_excerpt(&next_query, 360)
                ));
                candidate =
                    resolve_query_placeholders(&ensure_graphql_document(&next_query), datasets);
                continue;
            }

            if transport_error {
                repair_trace.push(
                    "[REPAIR] stopping repair because backend transport/connectivity failed"
                        .to_string(),
                );
                break;
            }

            if attempt >= MAX_REPAIR_ATTEMPTS {
                break;
            }
            let Some(repaired_query) = repair_multistep_query_with_llm(
                state,
                schema_registry,
                model_name,
                user_message,
                &last_query,
                &last_err,
                datasets,
                expected_root,
            )
            .await
            else {
                repair_trace.push("[REPAIR] LLM repair unavailable/failed".to_string());
                break;
            };
            repair_trace.push(format!(
                "[REPAIR] next_query(llm)={}",
                one_line_excerpt(&repaired_query, 360)
            ));
            let repaired_query = preserve_identifier_eq_semantics(&last_query, &repaired_query)
                .unwrap_or(repaired_query);
            if expected_root.is_some()
                && introduces_new_step_scope_literals(&last_query, &repaired_query)
            {
                repair_trace.push(
                    "[REPAIR] rejected llm query because it introduced new out-of-step scope literals"
                        .to_string(),
                );
                break;
            }
            candidate =
                resolve_query_placeholders(&ensure_graphql_document(&repaired_query), datasets);
        }

        let trace_block = if repair_trace.is_empty() {
            String::new()
        } else {
            format!("\nRepair trace:\n{}", repair_trace.join("\n"))
        };
        let message = format!(
            "Multi-step execution error: {last_err}\nQuery:\n```graphql\n{}\n```{}",
            last_query, trace_block
        );
        if last_err.starts_with("Multi-step query validation failed") {
            Err(PipelineError::validation(message))
        } else {
            Err(PipelineError::execution(message))
        }
    }

    let mut effective_queries = Vec::new();
    let mut debug_logs = Vec::new();
    let mut datasets: std::collections::HashMap<String, Vec<serde_json::Value>> =
        std::collections::HashMap::new();
    let mut step_roots: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut fetched_step_ids: Vec<String> = Vec::new();
    let mut fetch_query_idx = 0usize;
    let field_roles_for_root = |root: &str| schema_registry.field_roles_for_root(root);
    let merge_roles = |a: &FieldRoleSet, b: &FieldRoleSet| a.merge(b);
    let location_hints_for_roles = |roles: &FieldRoleSet| {
        LocationFieldHints::from_schema_lists(
            &roles.latitude_fields,
            &roles.longitude_fields,
            &roles.geo_object_fields,
        )
    };
    let record_hints_for_roles = |roles: &FieldRoleSet| {
        RecordFieldHints::from_schema_lists(
            &roles.time_fields,
            &roles.entity_key_fields,
            &roles.label_fields,
        )
    };

    let mut last_result: Option<Vec<serde_json::Value>> = None;
    let mut saw_post_fetch_step = false;
    let mut execution_groundings: Vec<ExecutionGrounding> = Vec::new();

    for step in &plan.steps {
        emit_progress(
            progress,
            PipelineProgressEvent::step(
                "running",
                &step.id,
                executor_op_kind_name(&step.op),
                None,
                None,
            ),
        );
        match &step.op {
            PlanV2Op::Fetch { filter, .. } => {
                let query = step.query.as_deref().ok_or_else(|| {
                    PipelineError::planning(format!(
                        "compiled fetch step `{}` is missing its GraphQL query",
                        step.id
                    ))
                })?;
                let expected_root = extract_root_field_from_query(query);
                let (body, effective_query, repair_trace) = run_graphql_query(
                    state,
                    schema_registry,
                    model_name,
                    user_message,
                    query,
                    &datasets,
                    expected_root.as_deref(),
                )
                .await?;
                fetch_query_idx += 1;
                effective_queries.push(ExecutedArtifact::query(
                    format!("Query {} ({}) (effective)", fetch_query_idx, step.id),
                    effective_query.clone(),
                ));
                if !repair_trace.is_empty() {
                    debug_logs.push(format!(
                        "[QUERY_REPAIR_TRACE] {}:\n{}",
                        step.id,
                        repair_trace.join("\n")
                    ));
                }
                let root_field = extract_root_field_from_query(&effective_query).or(expected_root);
                let Some(root_field) = root_field else {
                    emit_progress(
                        progress,
                        PipelineProgressEvent::step(
                            "completed",
                            &step.id,
                            executor_op_kind_name(&step.op),
                            None,
                            Some(0),
                        ),
                    );
                    continue;
                };
                let rows = body
                    .get("data")
                    .and_then(|d| d.get(&root_field))
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                execution_groundings.extend(execution_groundings_for_fetch(
                    schema_registry,
                    &root_field,
                    filter.as_ref(),
                    &rows,
                ));
                let rows_len = rows.len();
                step_roots.insert(step.id.clone(), root_field.clone());
                datasets.insert(step.id.clone(), rows);
                fetched_step_ids.push(step.id.clone());
                debug_logs.push(format!(
                    "[STEP_OUTPUT] {} (query {}) → {} row(s)",
                    step.id, root_field, rows_len
                ));
                emit_progress(
                    progress,
                    PipelineProgressEvent::step(
                        "completed",
                        &step.id,
                        executor_op_kind_name(&step.op),
                        Some(&root_field),
                        Some(rows_len),
                    ),
                );
                if rows_len > 0 {
                    let sample_rows = datasets
                        .get(&step.id)
                        .map(|r| r.iter().take(3).cloned().collect::<Vec<_>>())
                        .unwrap_or_default();
                    debug_logs.push(format!(
                        "[STEP_ROWS_SAMPLE] {} sample_rows={}:\n{}",
                        step.id,
                        sample_rows.len(),
                        serde_json::to_string_pretty(&sample_rows).unwrap_or_default()
                    ));
                }
            }
            PlanV2Op::Aggregate {
                source,
                group_by,
                metrics,
            } => {
                saw_post_fetch_step = true;
                if let Some(rows) = datasets.get(source) {
                    let mut out = aggregate_metrics(rows, group_by, metrics);
                    if group_by.is_empty()
                        && let Some(source_root) = step_roots.get(source)
                    {
                        let roles = field_roles_for_root(source_root);
                        carry_single_source_identity_fields(&mut out, rows, &roles);
                    }
                    if !group_by.is_empty()
                        && let Some(source_root) = step_roots.get(source).cloned()
                    {
                        hydrate_grouped_foreign_key_labels(
                            state,
                            schema_registry,
                            &source_root,
                            group_by,
                            &mut out,
                            &mut debug_logs,
                        )
                        .await;
                    }
                    let out_rows_len = out.len();
                    datasets.insert(step.id.clone(), out.clone());
                    debug_logs.push(format!(
                        "[STEP_OUTPUT] {} (aggregate {} metric(s) by {}) → {} row(s)",
                        step.id,
                        metrics.len(),
                        group_by.join(", "),
                        out_rows_len
                    ));
                    emit_progress(
                        progress,
                        PipelineProgressEvent::step(
                            "completed",
                            &step.id,
                            executor_op_kind_name(&step.op),
                            None,
                            Some(out_rows_len),
                        ),
                    );
                    last_result = Some(out);
                } else {
                    emit_progress(
                        progress,
                        PipelineProgressEvent::step(
                            "skipped",
                            &step.id,
                            executor_op_kind_name(&step.op),
                            None,
                            Some(0),
                        ),
                    );
                }
            }
            PlanV2Op::Compare {
                left,
                right,
                metric,
            } => {
                saw_post_fetch_step = true;
                if let (Some(left_rows), Some(right_rows)) =
                    (datasets.get(left), datasets.get(right))
                {
                    let left_root = step_roots.get(left).cloned().unwrap_or_default();
                    let right_root = step_roots.get(right).cloned().unwrap_or_default();
                    if !left_root.is_empty()
                        && left_root == right_root
                        && (left_rows.is_empty() || right_rows.is_empty())
                    {
                        let mut out = serde_json::Map::new();
                        out.insert(
                            "compare_error".to_string(),
                            serde_json::json!("comparison_sources_not_distinct"),
                        );
                        out.insert("left_source".to_string(), serde_json::json!(left));
                        out.insert("right_source".to_string(), serde_json::json!(right));
                        out.insert("left_root".to_string(), serde_json::json!(left_root));
                        out.insert("right_root".to_string(), serde_json::json!(right_root));
                        out.insert(
                            "message".to_string(),
                            serde_json::json!(
                                "Both comparison sources resolved to the same root query and one or both sources are empty, so comparison is not reliable."
                            ),
                        );
                        let out_rows = vec![serde_json::Value::Object(out)];
                        datasets.insert(step.id.clone(), out_rows.clone());
                        last_result = Some(out_rows);
                        emit_progress(
                            progress,
                            PipelineProgressEvent::step(
                                "completed",
                                &step.id,
                                executor_op_kind_name(&step.op),
                                None,
                                Some(1),
                            ),
                        );
                        continue;
                    }
                    let Some(metric) = metric else {
                        let mut out = serde_json::Map::new();
                        out.insert(
                            "compare_error".to_string(),
                            serde_json::json!("missing_metric"),
                        );
                        out.insert("left_source".to_string(), serde_json::json!(left));
                        out.insert("right_source".to_string(), serde_json::json!(right));
                        out.insert(
                            "message".to_string(),
                            serde_json::json!("Compare requires an explicit metric."),
                        );
                        let out_rows = vec![serde_json::Value::Object(out)];
                        datasets.insert(step.id.clone(), out_rows.clone());
                        last_result = Some(out_rows);
                        emit_progress(
                            progress,
                            PipelineProgressEvent::step(
                                "completed",
                                &step.id,
                                executor_op_kind_name(&step.op),
                                None,
                                Some(1),
                            ),
                        );
                        continue;
                    };
                    let out_rows = compare_rows(left, right, metric, left_rows, right_rows);
                    let out_rows_len = out_rows.len();
                    datasets.insert(step.id.clone(), out_rows.clone());
                    debug_logs.push(format!(
                        "[STEP_OUTPUT] {} (compare {} vs {}) → {} row(s)",
                        step.id, left, right, out_rows_len
                    ));
                    emit_progress(
                        progress,
                        PipelineProgressEvent::step(
                            "completed",
                            &step.id,
                            executor_op_kind_name(&step.op),
                            None,
                            Some(out_rows_len),
                        ),
                    );
                    last_result = Some(out_rows);
                } else {
                    emit_progress(
                        progress,
                        PipelineProgressEvent::step(
                            "skipped",
                            &step.id,
                            executor_op_kind_name(&step.op),
                            None,
                            Some(0),
                        ),
                    );
                }
            }
            PlanV2Op::FilterRows {
                source,
                field,
                operator,
                value,
            } => {
                saw_post_fetch_step = true;
                if let Some(rows) = datasets.get(source) {
                    let out = filter_rows(rows, field, operator, value);
                    let out_rows_len = out.len();
                    datasets.insert(step.id.clone(), out.clone());
                    debug_logs.push(format!(
                        "[STEP_OUTPUT] {} (filter {} {} {}) → {} row(s)",
                        step.id, field, operator, value, out_rows_len
                    ));
                    emit_progress(
                        progress,
                        PipelineProgressEvent::step(
                            "completed",
                            &step.id,
                            executor_op_kind_name(&step.op),
                            None,
                            Some(out_rows_len),
                        ),
                    );
                    last_result = Some(out);
                } else {
                    emit_progress(
                        progress,
                        PipelineProgressEvent::step(
                            "skipped",
                            &step.id,
                            executor_op_kind_name(&step.op),
                            None,
                            Some(0),
                        ),
                    );
                }
            }
            PlanV2Op::DistanceHaversine {
                vessels_source,
                target_source,
            } => {
                saw_post_fetch_step = true;
                if let (Some(vessel_rows), Some(target_rows)) =
                    (datasets.get(vessels_source), datasets.get(target_source))
                {
                    let vessel_info = vessel_rows
                        .iter()
                        .take(1)
                        .map(|r| {
                            format!(
                                "Vessel: {}",
                                serde_json::to_string_pretty(r)
                                    .unwrap_or_default()
                                    .lines()
                                    .take(8)
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n");

                    let target_info = target_rows
                        .iter()
                        .take(1)
                        .map(|r| {
                            format!(
                                "Target: {}",
                                serde_json::to_string_pretty(r)
                                    .unwrap_or_default()
                                    .lines()
                                    .take(8)
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n");

                    debug_logs.push(format!(
                        "\n[DATA_RETRIEVED] Distance computation using {} vessel row(s) and {} target row(s)",
                        vessel_rows.len(),
                        target_rows.len()
                    ));
                    debug_logs.push(format!("[VESSEL_DATA]\n{}", vessel_info));
                    debug_logs.push(format!("[TARGET_DATA]\n{}", target_info));

                    let vessel_root = step_roots.get(vessels_source).cloned().unwrap_or_default();
                    let target_root = step_roots.get(target_source).cloned().unwrap_or_default();
                    let vessel_roles = field_roles_for_root(&vessel_root);
                    let target_roles = field_roles_for_root(&target_root);
                    let merged_roles = merge_roles(&vessel_roles, &target_roles);
                    let location_field_hints = location_hints_for_roles(&merged_roles);
                    let record_field_hints = record_hints_for_roles(&merged_roles);
                    let out = compute_distance_rows(
                        vessel_rows,
                        target_rows,
                        Some(&location_field_hints),
                        Some(&record_field_hints),
                    );

                    if let Some(first_result) = out.first() {
                        debug_logs.push(format!(
                            "[DISTANCE_RESULT]\n{}",
                            serde_json::to_string_pretty(first_result).unwrap_or_default()
                        ));
                    }

                    datasets.insert(step.id.clone(), out.clone());
                    let out_rows_len = out.len();
                    emit_progress(
                        progress,
                        PipelineProgressEvent::step(
                            "completed",
                            &step.id,
                            executor_op_kind_name(&step.op),
                            None,
                            Some(out_rows_len),
                        ),
                    );
                    last_result = Some(out);
                } else {
                    emit_progress(
                        progress,
                        PipelineProgressEvent::step(
                            "skipped",
                            &step.id,
                            executor_op_kind_name(&step.op),
                            None,
                            Some(0),
                        ),
                    );
                }
            }
            PlanV2Op::JoinOnTime {
                left,
                right,
                left_time_field,
                right_time_field,
                window_minutes,
            } => {
                saw_post_fetch_step = true;
                if let (Some(left_rows), Some(right_rows)) =
                    (datasets.get(left), datasets.get(right))
                {
                    let left_root = step_roots.get(left).cloned().unwrap_or_default();
                    let right_root = step_roots.get(right).cloned().unwrap_or_default();
                    let left_roles = field_roles_for_root(&left_root);
                    let right_roles = field_roles_for_root(&right_root);
                    let merged_roles = merge_roles(&left_roles, &right_roles);
                    let record_field_hints = record_hints_for_roles(&merged_roles);
                    let out = join_on_time_rows(
                        left_rows,
                        right_rows,
                        left_time_field.as_deref(),
                        right_time_field.as_deref(),
                        *window_minutes,
                        Some(&record_field_hints),
                    );
                    let out_rows_len = out.len();
                    datasets.insert(step.id.clone(), out.clone());
                    debug_logs.push(format!(
                        "[STEP_OUTPUT] {} (join {} and {} on time, window {}min) → {} row(s)",
                        step.id,
                        left,
                        right,
                        window_minutes.unwrap_or(0),
                        out_rows_len
                    ));
                    emit_progress(
                        progress,
                        PipelineProgressEvent::step(
                            "completed",
                            &step.id,
                            executor_op_kind_name(&step.op),
                            None,
                            Some(out_rows_len),
                        ),
                    );
                    last_result = Some(out);
                } else {
                    emit_progress(
                        progress,
                        PipelineProgressEvent::step(
                            "skipped",
                            &step.id,
                            executor_op_kind_name(&step.op),
                            None,
                            Some(0),
                        ),
                    );
                }
            }
            PlanV2Op::Rank {
                source,
                by,
                direction,
                limit,
            } => {
                saw_post_fetch_step = true;
                if let Some(rows) = datasets.get(source) {
                    let dir = direction.as_deref().unwrap_or("desc");
                    let out = rank_rows(rows, by, dir, *limit);
                    let out_rows_len = out.len();
                    datasets.insert(step.id.clone(), out.clone());
                    debug_logs.push(format!(
                        "[STEP_OUTPUT] {} (rank by {} {}) → {} row(s)",
                        step.id, by, dir, out_rows_len
                    ));
                    emit_progress(
                        progress,
                        PipelineProgressEvent::step(
                            "completed",
                            &step.id,
                            executor_op_kind_name(&step.op),
                            None,
                            Some(out_rows_len),
                        ),
                    );
                    last_result = Some(out);
                } else {
                    emit_progress(
                        progress,
                        PipelineProgressEvent::step(
                            "skipped",
                            &step.id,
                            executor_op_kind_name(&step.op),
                            None,
                            Some(0),
                        ),
                    );
                }
            }
            PlanV2Op::ThresholdCheck {
                source,
                field,
                operator,
                value,
            } => {
                saw_post_fetch_step = true;
                if let Some(rows) = datasets.get(source) {
                    let out = threshold_check_rows(rows, field, operator, *value);
                    let out_rows_len = out.len();
                    datasets.insert(step.id.clone(), out.clone());
                    debug_logs.push(format!(
                        "[STEP_OUTPUT] {} (threshold check {} {} {}) → {} row(s)",
                        step.id, field, operator, value, out_rows_len
                    ));
                    emit_progress(
                        progress,
                        PipelineProgressEvent::step(
                            "completed",
                            &step.id,
                            executor_op_kind_name(&step.op),
                            None,
                            Some(out_rows_len),
                        ),
                    );
                    last_result = Some(out);
                } else {
                    emit_progress(
                        progress,
                        PipelineProgressEvent::step(
                            "skipped",
                            &step.id,
                            executor_op_kind_name(&step.op),
                            None,
                            Some(0),
                        ),
                    );
                }
            }
            PlanV2Op::TrendSummary {
                source,
                time_field,
                value_field,
            } => {
                saw_post_fetch_step = true;
                if let Some(rows) = datasets.get(source) {
                    let out = summarize_trend_rows(rows, time_field, value_field);
                    let out_rows_len = out.len();
                    datasets.insert(step.id.clone(), out.clone());
                    debug_logs.push(format!(
                        "[STEP_OUTPUT] {} (trend {} over {}) → {} row(s)",
                        step.id, value_field, time_field, out_rows_len
                    ));
                    emit_progress(
                        progress,
                        PipelineProgressEvent::step(
                            "completed",
                            &step.id,
                            executor_op_kind_name(&step.op),
                            None,
                            Some(out_rows_len),
                        ),
                    );
                    last_result = Some(out);
                } else {
                    emit_progress(
                        progress,
                        PipelineProgressEvent::step(
                            "skipped",
                            &step.id,
                            executor_op_kind_name(&step.op),
                            None,
                            Some(0),
                        ),
                    );
                }
            }
        }
    }

    let evidence_rows = select_evidence_rows(&datasets, &fetched_step_ids, last_result.as_ref());
    let evidence = build_execution_evidence(&evidence_rows);
    let empty_relation_guards = detect_empty_parent_rewrite_relations(plan, &datasets);

    if let Some(rows) = &last_result {
        // Add debug logs to the response if any were collected
        if !debug_logs.is_empty() {
            effective_queries.push(ExecutedArtifact::debug_log(
                "DEBUG_PREP_LOGS",
                debug_logs.join("\n"),
            ));
        }
        if !empty_relation_guards.is_empty() {
            if !saw_post_fetch_step
                && let Some(last_fetch_step_id) = fetched_step_ids.last()
                && let Some(answer) = fetch_summary_with_empty_relation_note(
                    plan,
                    last_fetch_step_id,
                    rows,
                    &empty_relation_guards,
                )
            {
                return Ok((
                    DeterministicAnswer::new(answer, DeterministicAnswerKind::RowList),
                    effective_queries,
                    evidence,
                    execution_groundings,
                ));
            }
            return Ok((
                DeterministicAnswer::new(
                    scope_guard_message(&empty_relation_guards),
                    DeterministicAnswerKind::Diagnostic,
                ),
                effective_queries,
                evidence,
                execution_groundings,
            ));
        }

        let has_metrics = rows.iter().any(row_has_metric_keys);
        if has_metrics {
            return Ok((
                DeterministicAnswer::new(
                    render_aggregate_result_summary(rows),
                    DeterministicAnswerKind::MetricSummary,
                ),
                effective_queries,
                evidence,
                execution_groundings,
            ));
        }
        let has_distance = rows.iter().any(|r| r.get("distanceKm").is_some());
        if has_distance {
            let top = rows
                .iter()
                .take(3)
                .map(|r| {
                    let label = r
                        .as_object()
                        .and_then(|obj| {
                            obj.get("target_name")
                                .and_then(|v| v.as_str())
                                .map(|name| {
                                    obj.get("target_shortName")
                                        .and_then(|v| v.as_str())
                                        .map(|short| format!("{name} ({short})"))
                                        .unwrap_or_else(|| name.to_string())
                                })
                                .or_else(|| {
                                    obj.get("target_shortName")
                                        .and_then(|v| v.as_str())
                                        .map(str::to_string)
                                })
                                .or_else(|| {
                                    let mut keys = obj.keys().cloned().collect::<Vec<_>>();
                                    keys.sort();
                                    keys.into_iter().find_map(|k| {
                                        if k == "distanceKm" {
                                            return None;
                                        }
                                        obj.get(&k).and_then(|v| match v {
                                            serde_json::Value::String(s) => {
                                                Some(format!("{k}={s}"))
                                            }
                                            serde_json::Value::Number(_)
                                            | serde_json::Value::Bool(_) => {
                                                Some(format!("{k}={v}"))
                                            }
                                            _ => None,
                                        })
                                    })
                                })
                        })
                        .unwrap_or_else(|| "entity".to_string());
                    let km = r
                        .get("distanceKm")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(f64::NAN);
                    format!("{}: {:.2} km", label, km)
                })
                .collect::<Vec<_>>()
                .join(", ");
            return Ok((
                DeterministicAnswer::new(
                    format!("Nearest results by distance: {top}."),
                    DeterministicAnswerKind::DistanceSummary,
                ),
                effective_queries,
                evidence,
                execution_groundings,
            ));
        }
        if rows.len() == 1 && rows[0].get("compare_error").is_some() {
            let message = rows[0].get("message").and_then(|v| v.as_str()).unwrap_or(
                "Comparison sources resolved to the same dataset, so comparison is not reliable.",
            );
            let left_root = rows[0]
                .get("left_root")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let right_root = rows[0]
                .get("right_root")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            return Ok((
                DeterministicAnswer::new(
                    format!("{message} (left root: `{left_root}`, right root: `{right_root}`)."),
                    DeterministicAnswerKind::CompareSummary,
                ),
                effective_queries,
                evidence,
                execution_groundings,
            ));
        }
        if rows.len() == 1
            && rows[0].get("left_value").is_some()
            && rows[0].get("right_value").is_some()
        {
            let left_value = rows[0]
                .get("left_value")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let right_value = rows[0]
                .get("right_value")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let delta = rows[0].get("delta").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let metric = rows[0]
                .get("metric")
                .and_then(|v| v.as_str())
                .unwrap_or("metric");
            return Ok((
                DeterministicAnswer::new(
                    format!(
                        "Comparison result ({metric}): left = {:.2}, right = {:.2}, delta = {:.2}.",
                        left_value, right_value, delta
                    ),
                    DeterministicAnswerKind::CompareSummary,
                ),
                effective_queries,
                evidence,
                execution_groundings,
            ));
        }
        if rows.len() == 1
            && rows[0].get("trend_direction").is_some()
            && rows[0].get("value_field").is_some()
        {
            return Ok((
                DeterministicAnswer::new(
                    render_trend_summary(rows),
                    DeterministicAnswerKind::TrendSummary,
                ),
                effective_queries,
                evidence,
                execution_groundings,
            ));
        }
        return Ok((
            DeterministicAnswer::new(
                render_rows_summary(
                    rows,
                    row_display_limit(user_message, rows.len(), DEFAULT_TRANSFORM_PREVIEW_ROWS),
                ),
                DeterministicAnswerKind::RowList,
            ),
            effective_queries,
            evidence,
            execution_groundings,
        ));
    }

    // Fetch-only plans (no aggregate/rank/compare/distance/join/threshold step):
    // return deterministic row summary from fetched data instead of a generic snapshot.
    if !saw_post_fetch_step
        && let Some(last_fetch_step_id) = fetched_step_ids.last()
        && let Some(rows) = datasets.get(last_fetch_step_id)
    {
        if !debug_logs.is_empty() {
            effective_queries.push(ExecutedArtifact::debug_log(
                "DEBUG_PREP_LOGS",
                debug_logs.join("\n"),
            ));
        }
        if !empty_relation_guards.is_empty() {
            return Ok((
                DeterministicAnswer::new(
                    scope_guard_message(&empty_relation_guards),
                    DeterministicAnswerKind::RowList,
                ),
                effective_queries,
                evidence,
                execution_groundings,
            ));
        }
        // For fetch-only responses, show all returned rows (capped) instead of a compact 3-row preview.
        let sample_limit = row_display_limit(user_message, rows.len(), DEFAULT_FETCH_PREVIEW_ROWS);
        let summary = step_roots
            .get(last_fetch_step_id)
            .map(|root| {
                let hints = row_display_hints_for_fetch(schema_registry, root, rows);
                render_rows_summary_with_hints(rows, sample_limit, &hints)
            })
            .unwrap_or_else(|| render_rows_summary(rows, sample_limit));
        return Ok((
            DeterministicAnswer::new(summary, DeterministicAnswerKind::RowList),
            effective_queries,
            evidence,
            execution_groundings,
        ));
    }

    let total_rows = datasets.values().map(|v| v.len()).sum::<usize>();
    let mut snapshots = Vec::new();
    for (step_id, rows) in datasets.iter().take(3) {
        let sample_rows = rows.iter().take(3).cloned().collect::<Vec<_>>();
        snapshots.push(format!(
            "{step_id}: {} row(s), sample: {}",
            rows.len(),
            render_rows_compact_summary(&sample_rows)
        ));
    }
    let snapshot_text = if snapshots.is_empty() {
        "no row snapshots available".to_string()
    } else {
        snapshots.join(" | ")
    };
    Ok((
        DeterministicAnswer::new(
            format!(
                "Executed multi-step plan (queries: {}, rows retrieved: {}). Data snapshots: {}.",
                fetch_query_idx, total_rows, snapshot_text
            ),
            DeterministicAnswerKind::Diagnostic,
        ),
        effective_queries,
        evidence,
        execution_groundings,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner::{ExecutableStep, MultiStepPlan};
    use crate::schema_registry::SchemaRegistry;

    fn registry() -> SchemaRegistry {
        SchemaRegistry::new(include_str!("../schemas/consumer_schema.graphql"))
    }

    fn registry_with_root_label_override(root: &str, label_fields: &[&str]) -> SchemaRegistry {
        let mut field_roles_by_root = std::collections::HashMap::new();
        field_roles_by_root.insert(
            root.to_string(),
            crate::sls::FieldRoles {
                label_fields: label_fields.iter().map(|field| field.to_string()).collect(),
                ..crate::sls::FieldRoles::default()
            },
        );
        let sls = crate::sls::Sls {
            concepts: std::collections::HashMap::new(),
            metrics: None,
            field_roles: None,
            field_roles_by_type: std::collections::HashMap::new(),
            field_roles_by_root,
            preferred_join_paths: Vec::new(),
            canonical_field_defaults: crate::sls::CanonicalFieldDefaults::default(),
            intent_vocabulary: crate::sls::IntentVocabulary::default(),
            policies: None,
            derived: crate::sls::SlsDerived::default(),
        };
        SchemaRegistry::with_sls(
            include_str!("../schemas/consumer_schema.graphql"),
            Some(&sls),
        )
    }

    #[test]
    fn label_hydration_uses_sls_label_roles_without_name_shortname_fallback() {
        let registry = registry_with_root_label_override("queryOffshoreWindFarm", &["plantId"]);
        let labels = target_label_fields(&registry, "queryOffshoreWindFarm");

        assert_eq!(
            labels,
            vec!["plantId".to_string()],
            "expected label fields to come only from configured roles"
        );
        let row = serde_json::json!({
            "name": "Wind Farm 1",
            "shortName": "WF1",
            "plantId": "PLANT-1"
        });
        assert_eq!(
            preferred_label_for_row(&row, &registry, "queryOffshoreWindFarm").as_deref(),
            Some("PLANT-1"),
            "name/shortName should not override SLS/schema label roles"
        );
    }

    #[test]
    fn repaired_query_cannot_import_sibling_compare_literals() {
        let previous = r#"query { queryOffshoreWindTurbine(filter: { name: { eq: "turbine 115" } }) { name } }"#;
        let repaired = r#"query { queryOffshoreWindTurbine(filter: { or: [{ name: { eq: "turbine 115" } }, { name: { eq: "turbine 109" } }] }) { name } }"#;
        assert!(introduces_new_step_scope_literals(previous, repaired));
    }

    #[test]
    fn detects_transport_execution_errors() {
        assert!(is_transport_execution_error(
            "error sending request for url (http://localhost:8000/graphql)"
        ));
        assert!(is_transport_execution_error("connection refused"));
        assert!(!is_transport_execution_error(
            "GraphQL execution errors: Cannot query field `foo` on type `Query`."
        ));
    }

    #[test]
    fn row_display_limit_expands_only_for_explicit_full_requests() {
        assert_eq!(
            row_display_limit("List turbines with downtime greater than 400", 30, 3),
            3
        );
        assert_eq!(
            row_display_limit("List all turbines with downtime greater than 400", 30, 3),
            30
        );
        assert_eq!(
            row_display_limit("Show all matching turbines", 5000, 3),
            EXPLICIT_FULL_PREVIEW_ROWS
        );
    }

    #[test]
    fn select_evidence_rows_prefers_transformed_result_over_raw_fetch() {
        let mut datasets = std::collections::HashMap::new();
        datasets.insert(
            "s1".to_string(),
            vec![
                serde_json::json!({"name": "raw"}),
                serde_json::json!({"name": "also raw"}),
            ],
        );
        let filtered = vec![serde_json::json!({"name": "filtered"})];

        let evidence = select_evidence_rows(&datasets, &["s1".to_string()], Some(&filtered));

        assert_eq!(evidence, filtered);
    }

    #[test]
    fn compare_same_root_is_allowed_when_both_sources_have_rows() {
        let left = vec![serde_json::json!({"accumulatedDowntime": 489.18})];
        let right = vec![serde_json::json!({"accumulatedDowntime": 417.72})];
        let metric = crate::planner::MetricSpec::Avg {
            field: "accumulatedDowntime".to_string(),
        };
        let out = compare_rows("s1", "s2", &metric, &left, &right);
        assert_eq!(out.len(), 1);
        assert!(out[0].get("compare_error").is_none());
    }

    #[test]
    fn compare_evidence_includes_both_source_datasets() {
        let mut datasets = std::collections::HashMap::new();
        datasets.insert(
            "s2".to_string(),
            vec![serde_json::json!({"name": "Turbine 115", "accumulatedDowntime": 489.18})],
        );
        datasets.insert(
            "s4".to_string(),
            vec![serde_json::json!({"name": "Turbine 109", "accumulatedDowntime": 417.72})],
        );
        let compare_rows = vec![serde_json::json!({
            "left_source": "s2",
            "right_source": "s4",
            "left_value": 489.18,
            "right_value": 417.72,
            "metric": "avg(accumulatedDowntime)"
        })];
        let evidence_rows = select_evidence_rows(&datasets, &[], Some(&compare_rows));
        let evidence = build_execution_evidence(&evidence_rows);
        assert_eq!(evidence.row_count, 2);
        assert!(
            evidence
                .field_values
                .get("name")
                .is_some_and(|values| values.iter().any(|v| v == "Turbine 115"))
        );
        assert!(
            evidence
                .field_values
                .get("name")
                .is_some_and(|values| values.iter().any(|v| v == "Turbine 109"))
        );
    }

    #[test]
    fn aggregate_rows_carry_single_source_identity_fields() {
        let source_rows = vec![serde_json::json!({
            "name": "Turbine 115",
            "shortName": "T115",
            "accumulatedDowntime": 489.18
        })];
        let mut aggregate_rows = vec![serde_json::json!({
            "avg_accumulatedDowntime": 489.18
        })];
        let roles = FieldRoleSet {
            label_fields: vec!["name".to_string(), "shortName".to_string()],
            ..FieldRoleSet::default()
        };

        carry_single_source_identity_fields(&mut aggregate_rows, &source_rows, &roles);

        let row = aggregate_rows[0].as_object().expect("aggregate row object");
        assert_eq!(
            row.get("name").and_then(|v| v.as_str()),
            Some("Turbine 115")
        );
        assert_eq!(row.get("shortName").and_then(|v| v.as_str()), Some("T115"));
        assert_eq!(
            row.get("avg_accumulatedDowntime").and_then(|v| v.as_f64()),
            Some(489.18)
        );
    }

    #[test]
    fn detects_empty_child_relation_after_parent_rewrite() {
        let plan = MultiStepPlan {
            rewrites: "parent_relation_rewrite".to_string(),
            notes: String::new(),
            execute_error: String::new(),
            steps: vec![ExecutableStep {
                id: "s1".to_string(),
                description: "Fetch".to_string(),
                query: None,
                op: PlanV2Op::Fetch {
                    root_field: "queryOffshoreWindFarm".to_string(),
                    fields: vec![
                        "name".to_string(),
                        "shortName".to_string(),
                        "hasOffshoreWindTurbine.name".to_string(),
                        "hasOffshoreWindTurbine.shortName".to_string(),
                    ],
                    first: Some(2000),
                    offset: None,
                    filter: None,
                    order: None,
                },
            }],
        };
        let rows = vec![serde_json::json!({
            "name": "Wind Farm 1",
            "shortName": "WF1",
            "hasOffshoreWindTurbine": []
        })];
        let mut datasets = std::collections::HashMap::new();
        datasets.insert("s1".to_string(), rows);

        let guards = detect_empty_parent_rewrite_relations(&plan, &datasets);

        assert_eq!(
            guards,
            vec!["queryOffshoreWindFarm.hasOffshoreWindTurbine returned no child rows"]
        );
    }

    #[test]
    fn detects_empty_child_relation_without_parent_rewrite_marker() {
        let plan = MultiStepPlan {
            rewrites: String::new(),
            notes: String::new(),
            execute_error: String::new(),
            steps: vec![ExecutableStep {
                id: "s1".to_string(),
                description: "Fetch".to_string(),
                query: None,
                op: PlanV2Op::Fetch {
                    root_field: "queryOffshoreWindFarm".to_string(),
                    fields: vec![
                        "name".to_string(),
                        "hasOffshoreWindTurbine.name".to_string(),
                        "hasOffshoreWindTurbine.shortName".to_string(),
                    ],
                    first: Some(20),
                    offset: None,
                    filter: None,
                    order: None,
                },
            }],
        };
        let rows = vec![serde_json::json!({
            "name": "Wind Farm 1",
            "hasOffshoreWindTurbine": []
        })];
        let mut datasets = std::collections::HashMap::new();
        datasets.insert("s1".to_string(), rows);

        let guards = detect_empty_parent_rewrite_relations(&plan, &datasets);

        assert_eq!(
            guards,
            vec!["queryOffshoreWindFarm.hasOffshoreWindTurbine returned no child rows"]
        );
    }

    #[test]
    fn ignores_pruned_relation_field_that_is_absent_from_rows() {
        let plan = MultiStepPlan {
            rewrites: String::new(),
            notes: String::new(),
            execute_error: String::new(),
            steps: vec![ExecutableStep {
                id: "s1".to_string(),
                description: "Fetch".to_string(),
                query: None,
                op: PlanV2Op::Fetch {
                    root_field: "queryOffshoreWindTurbine".to_string(),
                    fields: vec![
                        "name".to_string(),
                        "shortName".to_string(),
                        "accumulatedDowntime".to_string(),
                        "location.point.latitude".to_string(),
                    ],
                    first: Some(20),
                    offset: None,
                    filter: None,
                    order: None,
                },
            }],
        };
        let rows = vec![serde_json::json!({
            "name": "Turbine 120",
            "shortName": "T120",
            "accumulatedDowntime": 459.18
        })];
        let mut datasets = std::collections::HashMap::new();
        datasets.insert("s1".to_string(), rows);

        let guards = detect_empty_parent_rewrite_relations(&plan, &datasets);

        assert!(guards.is_empty());
    }

    #[test]
    fn ignores_non_empty_child_relation_after_parent_rewrite() {
        let plan = MultiStepPlan {
            rewrites: "parent_relation_rewrite".to_string(),
            notes: String::new(),
            execute_error: String::new(),
            steps: vec![ExecutableStep {
                id: "s1".to_string(),
                description: "Fetch".to_string(),
                query: None,
                op: PlanV2Op::Fetch {
                    root_field: "queryOffshoreWindFarm".to_string(),
                    fields: vec!["hasOffshoreWindTurbine.name".to_string()],
                    first: Some(2000),
                    offset: None,
                    filter: None,
                    order: None,
                },
            }],
        };
        let rows = vec![serde_json::json!({
            "hasOffshoreWindTurbine": [
                { "name": "Turbine 1" }
            ]
        })];
        let mut datasets = std::collections::HashMap::new();
        datasets.insert("s1".to_string(), rows);

        let guards = detect_empty_parent_rewrite_relations(&plan, &datasets);

        assert!(guards.is_empty());
    }

    #[test]
    fn detects_empty_child_relation_across_multiple_fetch_steps() {
        let plan = MultiStepPlan {
            rewrites: "parent_relation_rewrite".to_string(),
            notes: String::new(),
            execute_error: String::new(),
            steps: vec![
                ExecutableStep {
                    id: "s1".to_string(),
                    description: "Fetch farm 1".to_string(),
                    query: None,
                    op: PlanV2Op::Fetch {
                        root_field: "queryOffshoreWindFarm".to_string(),
                        fields: vec!["hasOffshoreWindTurbine.accumulatedDowntime".to_string()],
                        first: Some(20),
                        offset: None,
                        filter: None,
                        order: None,
                    },
                },
                ExecutableStep {
                    id: "s2".to_string(),
                    description: "Fetch farm 2".to_string(),
                    query: None,
                    op: PlanV2Op::Fetch {
                        root_field: "queryOffshoreWindFarm".to_string(),
                        fields: vec!["hasOffshoreWindTurbine.accumulatedDowntime".to_string()],
                        first: Some(20),
                        offset: None,
                        filter: None,
                        order: None,
                    },
                },
            ],
        };
        let mut datasets = std::collections::HashMap::new();
        datasets.insert(
            "s1".to_string(),
            vec![serde_json::json!({"hasOffshoreWindTurbine": []})],
        );
        datasets.insert(
            "s2".to_string(),
            vec![serde_json::json!({"hasOffshoreWindTurbine": []})],
        );

        let guards = detect_empty_parent_rewrite_relations(&plan, &datasets);

        assert_eq!(
            guards,
            vec!["queryOffshoreWindFarm.hasOffshoreWindTurbine returned no child rows"]
        );
    }

    #[test]
    fn grouped_target_root_is_derived_from_relation_uid_field() {
        let schema = registry();
        let root = grouped_target_root_for_field(
            &schema,
            "queryOffshoreWindTurbine",
            "partOfOffshoreWindFarmUid",
        );

        assert_eq!(root.as_deref(), Some("queryOffshoreWindFarm"));
    }

    #[test]
    fn fetch_summary_with_empty_relation_note_keeps_parent_details_when_scalars_exist() {
        let plan = MultiStepPlan {
            rewrites: String::new(),
            notes: String::new(),
            execute_error: String::new(),
            steps: vec![ExecutableStep {
                id: "s1".to_string(),
                description: "Fetch".to_string(),
                query: None,
                op: PlanV2Op::Fetch {
                    root_field: "queryOffshoreWindFarm".to_string(),
                    fields: vec![
                        "name".to_string(),
                        "shortName".to_string(),
                        "plantId".to_string(),
                        "hasOffshoreWindTurbine.name".to_string(),
                    ],
                    first: Some(20),
                    offset: None,
                    filter: None,
                    order: None,
                },
            }],
        };
        let rows = vec![serde_json::json!({
            "name": "Wind Farm 1",
            "shortName": "WF1",
            "plantId": "PLANT-  1",
            "hasOffshoreWindTurbine": []
        })];
        let answer = fetch_summary_with_empty_relation_note(
            &plan,
            "s1",
            &rows,
            &["queryOffshoreWindFarm.hasOffshoreWindTurbine returned no child rows".to_string()],
        )
        .expect("expected parent detail summary");

        assert!(answer.contains("Found 1 result(s):"));
        assert!(answer.contains("name: Wind Farm 1"));
        assert!(answer.contains("shortName: WF1"));
        assert!(answer.contains("Note: the backend returned no child rows"));
    }

    #[test]
    fn fetch_summary_with_empty_relation_note_stays_none_for_child_membership_queries() {
        let plan = MultiStepPlan {
            rewrites: String::new(),
            notes: String::new(),
            execute_error: String::new(),
            steps: vec![ExecutableStep {
                id: "s1".to_string(),
                description: "Fetch".to_string(),
                query: None,
                op: PlanV2Op::Fetch {
                    root_field: "queryOffshoreWindFarm".to_string(),
                    fields: vec![
                        "name".to_string(),
                        "hasOffshoreWindTurbine.name".to_string(),
                        "hasOffshoreWindTurbine.shortName".to_string(),
                    ],
                    first: Some(20),
                    offset: None,
                    filter: None,
                    order: None,
                },
            }],
        };
        let rows = vec![serde_json::json!({
            "name": "Wind Farm 1",
            "hasOffshoreWindTurbine": []
        })];

        assert!(
            fetch_summary_with_empty_relation_note(
                &plan,
                "s1",
                &rows,
                &[
                    "queryOffshoreWindFarm.hasOffshoreWindTurbine returned no child rows"
                        .to_string()
                ],
            )
            .is_none()
        );
    }
}
