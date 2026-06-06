#![allow(
    clippy::format_push_string,
    clippy::match_same_arms,
    clippy::missing_const_for_fn,
    clippy::redundant_pub_crate,
    clippy::use_self
)]

use crate::error::{PipelineError, PipelineResult};
use crate::intermediate_representation::{IRQuery, ir_to_graphql};
use crate::metric_formula::parse_metric_formula;
use crate::schema_registry::SchemaRegistry;
use crate::sls::{IntentVocabulary, Metric, Sls};
use graphql_parser::query::{
    Definition as QueryDefinition, OperationDefinition, Selection, Value as QueryValue, parse_query,
};
use regex::Regex;
use serde::{Deserialize, Deserializer};
use std::collections::{HashMap, HashSet};
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ExecutedArtifactKind {
    Query,
    DebugLog,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct ExecutedArtifact {
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) kind: ExecutedArtifactKind,
}

impl ExecutedArtifact {
    pub(crate) fn query(title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            body: body.into(),
            kind: ExecutedArtifactKind::Query,
        }
    }

    pub(crate) fn debug_log(title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            body: body.into(),
            kind: ExecutedArtifactKind::DebugLog,
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct ExecutableStep {
    pub(crate) id: String,
    pub(crate) description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) query: Option<String>,
    #[serde(flatten)]
    pub(crate) op: PlanV2Op,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct PlanV2 {
    #[serde(default)]
    pub(crate) version: Option<String>,
    #[serde(default)]
    pub(crate) rewrites: Vec<String>,
    #[serde(default)]
    pub(crate) notes: Vec<String>,
    #[serde(default)]
    pub(crate) steps: Vec<PlanV2Step>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct PlanV2Step {
    #[serde(default)]
    pub(crate) id: String,
    #[serde(flatten)]
    pub(crate) op: PlanV2Op,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum MetricSpec {
    Count,
    Sum { field: String },
    Avg { field: String },
    Min { field: String },
    Max { field: String },
    Stddev { field: String },
    Ref { name: String },
    Formula { name: String, expr: String },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum MetricSpecWire {
    Count,
    Sum { field: String },
    Avg { field: String },
    Min { field: String },
    Max { field: String },
    Stddev { field: String },
    Metric { name: String },
    Formula { name: String, expr: String },
}

impl From<MetricSpecWire> for MetricSpec {
    fn from(value: MetricSpecWire) -> Self {
        match value {
            MetricSpecWire::Count => MetricSpec::Count,
            MetricSpecWire::Sum { field } => MetricSpec::Sum { field },
            MetricSpecWire::Avg { field } => MetricSpec::Avg { field },
            MetricSpecWire::Min { field } => MetricSpec::Min { field },
            MetricSpecWire::Max { field } => MetricSpec::Max { field },
            MetricSpecWire::Stddev { field } => MetricSpec::Stddev { field },
            MetricSpecWire::Metric { name } => MetricSpec::Ref { name },
            MetricSpecWire::Formula { name, expr } => MetricSpec::Formula { name, expr },
        }
    }
}

impl<'de> Deserialize<'de> for MetricSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        match value {
            serde_json::Value::String(raw) => parse_metric_spec_from_str(&raw)
                .ok_or_else(|| serde::de::Error::custom("invalid metric string")),
            serde_json::Value::Object(_) => {
                let wire: MetricSpecWire =
                    serde_json::from_value(value).map_err(serde::de::Error::custom)?;
                Ok(wire.into())
            }
            _ => Err(serde::de::Error::custom(
                "metric must be a string or an object",
            )),
        }
    }
}

impl MetricSpec {
    pub(crate) fn requires_field(&self) -> bool {
        !matches!(
            self,
            MetricSpec::Count | MetricSpec::Ref { .. } | MetricSpec::Formula { .. }
        )
    }

    pub(crate) fn field(&self) -> Option<&str> {
        match self {
            MetricSpec::Count => None,
            MetricSpec::Ref { .. } => None,
            MetricSpec::Formula { .. } => None,
            MetricSpec::Sum { field }
            | MetricSpec::Avg { field }
            | MetricSpec::Min { field }
            | MetricSpec::Max { field }
            | MetricSpec::Stddev { field } => Some(field.as_str()),
        }
    }

    pub(crate) fn output_key(&self) -> String {
        match self {
            MetricSpec::Count => "count".to_string(),
            MetricSpec::Sum { field } => format!("sum_{}", normalize_metric_field(field)),
            MetricSpec::Avg { field } => format!("avg_{}", normalize_metric_field(field)),
            MetricSpec::Min { field } => format!("min_{}", normalize_metric_field(field)),
            MetricSpec::Max { field } => format!("max_{}", normalize_metric_field(field)),
            MetricSpec::Stddev { field } => format!("stddev_{}", normalize_metric_field(field)),
            MetricSpec::Ref { name } => format!("metric_{}", normalize_metric_field(name)),
            MetricSpec::Formula { name, .. } => format!("metric_{}", normalize_metric_field(name)),
        }
    }
}

impl fmt::Display for MetricSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MetricSpec::Count => write!(f, "count"),
            MetricSpec::Sum { field } => write!(f, "sum({field})"),
            MetricSpec::Avg { field } => write!(f, "avg({field})"),
            MetricSpec::Min { field } => write!(f, "min({field})"),
            MetricSpec::Max { field } => write!(f, "max({field})"),
            MetricSpec::Stddev { field } => write!(f, "stddev({field})"),
            MetricSpec::Ref { name } => write!(f, "metric:{name}"),
            MetricSpec::Formula { name, .. } => write!(f, "metric:{name}"),
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum PlanV2Op {
    Fetch {
        root_field: String,
        fields: Vec<String>,
        #[serde(default)]
        first: Option<i64>,
        #[serde(default)]
        offset: Option<i64>,
        #[serde(default)]
        filter: Option<serde_json::Value>,
        #[serde(default)]
        order: Option<serde_json::Value>,
    },
    Aggregate {
        source: String,
        #[serde(default)]
        group_by: Vec<String>,
        #[serde(default)]
        metrics: Vec<MetricSpec>,
    },
    Compare {
        left: String,
        right: String,
        #[serde(default)]
        metric: Option<MetricSpec>,
    },
    FilterRows {
        source: String,
        field: String,
        operator: String,
        value: serde_json::Value,
    },
    Rank {
        source: String,
        by: String,
        #[serde(default)]
        direction: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
    },
    DistanceHaversine {
        vessels_source: String,
        target_source: String,
    },
    JoinOnTime {
        left: String,
        right: String,
        #[serde(default)]
        left_time_field: Option<String>,
        #[serde(default)]
        right_time_field: Option<String>,
        #[serde(default)]
        window_minutes: Option<i64>,
    },
    ThresholdCheck {
        source: String,
        field: String,
        operator: String,
        value: f64,
    },
    TrendSummary {
        source: String,
        time_field: String,
        value_field: String,
    },
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct MultiStepPlan {
    pub(crate) rewrites: String,
    pub(crate) notes: String,
    pub(crate) steps: Vec<ExecutableStep>,
    #[serde(default)]
    pub(crate) execute_error: String,
}

pub(crate) fn render_multistep_plan(plan: &MultiStepPlan) -> String {
    let mut output = String::new();
    output.push_str(&format!("Rewrites: {}\n\n", plan.rewrites));
    output.push_str(&format!("Notes: {}\n\n", plan.notes));
    output.push_str("Plan:\n");
    for (idx, step) in plan.steps.iter().enumerate() {
        output.push_str(&format!("{}. {}\n", idx + 1, step.description));
    }

    let mut fetch_idx = 0usize;
    for step in &plan.steps {
        if let Some(query) = &step.query {
            fetch_idx += 1;
            output.push_str(&format!(
                "\nQuery {} ({}):\n```graphql\n{}\n```\n",
                fetch_idx, step.id, query
            ));
        }
    }
    output
}

pub(crate) fn render_effective_queries(artifacts: &[ExecutedArtifact]) -> String {
    if artifacts.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    out.push_str("Effective Executed Queries:\n");
    for artifact in artifacts {
        let fence = match artifact.kind {
            ExecutedArtifactKind::Query => "graphql",
            ExecutedArtifactKind::DebugLog => "text",
        };
        out.push_str(&format!(
            "\n{}:\n```{}\n{}\n```\n",
            artifact.title, fence, artifact.body
        ));
    }
    out
}

pub(crate) fn executed_query_text(artifacts: &[ExecutedArtifact]) -> String {
    artifacts
        .iter()
        .filter(|artifact| artifact.kind == ExecutedArtifactKind::Query)
        .map(|artifact| artifact.body.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn normalize_metric_field(field: &str) -> String {
    field
        .trim()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect::<String>()
        .trim_matches('_')
        .to_string()
}

fn parse_metric_spec_from_str(raw: &str) -> Option<MetricSpec> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower == "count" || lower == "count()" {
        return Some(MetricSpec::Count);
    }
    if let Some(name) = trimmed
        .strip_prefix("metric:")
        .or_else(|| trimmed.strip_prefix("sls:"))
    {
        let name = name.trim();
        if !name.is_empty() {
            return Some(MetricSpec::Ref {
                name: name.to_string(),
            });
        }
    }
    let mut op = "";
    let mut field: Option<String> = None;
    if let Some(idx) = trimmed.find('(') {
        if trimmed.ends_with(')') && idx + 1 < trimmed.len() - 1 {
            op = trimmed[..idx].trim();
            let inner = trimmed[idx + 1..trimmed.len() - 1].trim();
            if !inner.is_empty() {
                field = Some(inner.to_string());
            }
        }
    } else if let Some(idx) = trimmed.find(':') {
        op = trimmed[..idx].trim();
        let inner = trimmed[idx + 1..].trim();
        if !inner.is_empty() {
            field = Some(inner.to_string());
        }
    }
    let op_lower = op.to_ascii_lowercase();
    let field = field?;
    match op_lower.as_str() {
        "sum" => Some(MetricSpec::Sum { field }),
        "avg" | "average" | "mean" => Some(MetricSpec::Avg { field }),
        "min" => Some(MetricSpec::Min { field }),
        "max" => Some(MetricSpec::Max { field }),
        "stddev" | "stdev" | "std" => Some(MetricSpec::Stddev { field }),
        _ => None,
    }
}

fn parse_metric_from_aggregation(raw: &str) -> Option<MetricSpec> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower == "count" || lower.starts_with("count(") {
        return Some(MetricSpec::Count);
    }
    parse_metric_spec_from_str(trimmed)
}

fn metric_spec_to_formula(spec: &MetricSpec) -> String {
    match spec {
        MetricSpec::Count => "count()".to_string(),
        MetricSpec::Sum { field } => format!("sum({field})"),
        MetricSpec::Avg { field } => format!("avg({field})"),
        MetricSpec::Min { field } => format!("min({field})"),
        MetricSpec::Max { field } => format!("max({field})"),
        MetricSpec::Stddev { field } => format!("stddev({field})"),
        MetricSpec::Ref { name } => format!("metric:{name}"),
        MetricSpec::Formula { expr, .. } => expr.clone(),
    }
}

fn resolve_metric_ref(
    name: &str,
    sls_metrics: &std::collections::HashMap<String, Metric>,
) -> Result<MetricSpec, String> {
    let metric = sls_metrics
        .get(name)
        .or_else(|| {
            sls_metrics
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, v)| v)
        })
        .ok_or_else(|| format!("Unknown SLS metric `{name}`."))?;
    let aggregation = metric
        .aggregation
        .as_deref()
        .ok_or_else(|| format!("SLS metric `{name}` has no aggregation definition."))?;
    if parse_metric_formula(aggregation).is_ok() {
        return Ok(MetricSpec::Formula {
            name: name.to_string(),
            expr: aggregation.to_string(),
        });
    }
    if let Some(spec) = parse_metric_from_aggregation(aggregation) {
        let expr = metric_spec_to_formula(&spec);
        if parse_metric_formula(&expr).is_ok() {
            return Ok(MetricSpec::Formula {
                name: name.to_string(),
                expr,
            });
        }
    }
    Err(format!(
        "SLS metric `{name}` uses unsupported aggregation formula `{aggregation}`."
    ))
}

pub(crate) fn resolve_sls_metric_refs(plan: &mut PlanV2, sls: Option<&Sls>) -> Result<(), String> {
    let mut has_refs = false;
    for step in &plan.steps {
        match &step.op {
            PlanV2Op::Aggregate { metrics, .. }
                if metrics.iter().any(|m| matches!(m, MetricSpec::Ref { .. })) =>
            {
                has_refs = true;
            }
            PlanV2Op::Compare { metric, .. }
                if metric
                    .as_ref()
                    .is_some_and(|m| matches!(m, MetricSpec::Ref { .. })) =>
            {
                has_refs = true;
            }
            _ => {}
        }
    }
    if !has_refs {
        return Ok(());
    }
    let sls = sls.ok_or_else(|| "SLS metric references require loaded SLS.".to_string())?;
    let sls_metrics = sls
        .metrics
        .as_ref()
        .ok_or_else(|| "SLS metrics are not configured.".to_string())?;
    for step in &mut plan.steps {
        match &mut step.op {
            PlanV2Op::Aggregate { metrics, .. } => {
                for metric in metrics.iter_mut() {
                    if let MetricSpec::Ref { name } = metric {
                        let resolved = resolve_metric_ref(name, sls_metrics)?;
                        *metric = resolved;
                    }
                }
            }
            PlanV2Op::Compare { metric, .. } => {
                if let Some(metric) = metric
                    && let MetricSpec::Ref { name } = metric
                {
                    let resolved = resolve_metric_ref(name, sls_metrics)?;
                    *metric = resolved;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

pub(crate) fn validate_sls_metric_sources(
    plan: &PlanV2,
    schema_registry: &SchemaRegistry,
    sls: Option<&Sls>,
) -> Result<(), String> {
    let Some(sls) = sls else {
        return Ok(());
    };
    let Some(metrics) = sls.metrics.as_ref() else {
        return Ok(());
    };
    if metrics.is_empty() {
        return Ok(());
    }

    fn lookup_metric<'a>(metrics: &'a HashMap<String, Metric>, name: &str) -> Option<&'a Metric> {
        metrics.get(name).or_else(|| {
            metrics
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v)
        })
    }

    fn metric_name_if_sls(
        metric: &MetricSpec,
        metrics: &HashMap<String, Metric>,
    ) -> Option<String> {
        match metric {
            MetricSpec::Ref { name } | MetricSpec::Formula { name, .. } => {
                lookup_metric(metrics, name).map(|_| name.clone())
            }
            _ => None,
        }
    }

    fn build_origin_roots(plan: &PlanV2) -> HashMap<String, String> {
        let mut origins = HashMap::new();
        for step in &plan.steps {
            if let PlanV2Op::Fetch { root_field, .. } = &step.op {
                origins.insert(step.id.clone(), root_field.clone());
            }
        }
        for _ in 0..plan.steps.len() {
            let mut changed = false;
            for step in &plan.steps {
                if origins.contains_key(&step.id) {
                    continue;
                }
                match &step.op {
                    PlanV2Op::FilterRows { source, .. }
                    | PlanV2Op::Rank { source, .. }
                    | PlanV2Op::Aggregate { source, .. }
                    | PlanV2Op::ThresholdCheck { source, .. }
                    | PlanV2Op::TrendSummary { source, .. } => {
                        if let Some(root) = origins.get(source).cloned() {
                            origins.insert(step.id.clone(), root);
                            changed = true;
                        }
                    }
                    PlanV2Op::JoinOnTime { left, right, .. } => {
                        if let (Some(l), Some(r)) = (origins.get(left), origins.get(right))
                            && l.eq_ignore_ascii_case(r)
                        {
                            origins.insert(step.id.clone(), l.clone());
                            changed = true;
                        }
                    }
                    PlanV2Op::DistanceHaversine {
                        vessels_source,
                        target_source,
                    } => {
                        if let (Some(l), Some(r)) =
                            (origins.get(vessels_source), origins.get(target_source))
                            && l.eq_ignore_ascii_case(r)
                        {
                            origins.insert(step.id.clone(), l.clone());
                            changed = true;
                        }
                    }
                    _ => {}
                }
            }
            if !changed {
                break;
            }
        }
        origins
    }

    let origin_roots = build_origin_roots(plan);

    let validate_source = |step_id: &str, metric_name: &str| -> Result<(), String> {
        let Some(metric) = lookup_metric(metrics, metric_name) else {
            return Ok(());
        };
        let Some(root) = origin_roots.get(step_id) else {
            return Err(format!(
                "step '{step_id}': cannot validate metric '{metric_name}' because source root is unknown"
            ));
        };
        let Some(root_type) = schema_registry.query_return_type(root) else {
            return Err(format!(
                "step '{step_id}': cannot validate metric '{metric_name}' because root '{root}' has no return type"
            ));
        };
        if !root_type.eq_ignore_ascii_case(metric.source.type_name.as_str()) {
            return Err(format!(
                "step '{step_id}': metric '{metric_name}' expects source type '{}', but root '{root}' returns '{}'",
                metric.source.type_name, root_type
            ));
        }
        Ok(())
    };

    for step in &plan.steps {
        match &step.op {
            PlanV2Op::Aggregate {
                source,
                metrics: ms,
                ..
            } => {
                for metric in ms {
                    if let Some(name) = metric_name_if_sls(metric, metrics) {
                        validate_source(source, &name)?;
                    }
                }
            }
            PlanV2Op::Compare {
                left,
                right,
                metric,
            } => {
                if let Some(metric) = metric
                    && let Some(name) = metric_name_if_sls(metric, metrics)
                {
                    validate_source(left, &name)?;
                    validate_source(right, &name)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn extract_json_candidates(response: &str) -> Vec<String> {
    let mut candidates = vec![response.trim().to_string()];
    if let Some(start) = response.find("```json") {
        let rest = &response[start + 7..];
        if let Some(end) = rest.find("```") {
            candidates.push(rest[..end].trim().to_string());
        }
    } else if let Some(start) = response.find("```") {
        let rest = &response[start + 3..];
        if let Some(end) = rest.find("```") {
            candidates.push(rest[..end].trim().to_string());
        }
    }
    candidates
}

fn normalize_filter_rows_step_value(step: &mut serde_json::Map<String, serde_json::Value>) -> bool {
    let Some(op) = step.get("op").and_then(|value| value.as_str()) else {
        return false;
    };
    if op != "filter_rows" {
        return false;
    }

    if step.get("source").is_none()
        && let Some(input) = step.remove("input")
    {
        step.insert("source".to_string(), input);
    }
    if let Some(source) = step.get_mut("source")
        && let Some(source_ref) = source
            .as_object()
            .and_then(|obj| obj.get("ref"))
            .and_then(|value| value.as_str())
    {
        *source = serde_json::json!(source_ref);
    }
    if let Some(source) = step.get_mut("source")
        && let Some(source_ref) = source.as_str()
        && let Some(inner) = source_ref
            .trim()
            .strip_prefix("${")
            .and_then(|value| value.strip_suffix('}'))
        && !inner.contains('.')
        && !inner.trim().is_empty()
    {
        *source = serde_json::json!(inner.trim());
    }

    let needs_flattened_filter = step.get("field").is_none()
        || step.get("operator").is_none()
        || step.get("value").is_none();
    if !needs_flattened_filter {
        return true;
    }

    let Some(filter_value) = step.remove("filter").or_else(|| step.remove("condition")) else {
        return true;
    };
    let Some(filter_obj) = filter_value.as_object() else {
        step.insert("filter".to_string(), filter_value);
        return true;
    };
    if filter_obj.len() != 1 {
        step.insert("filter".to_string(), filter_value);
        return true;
    }

    let Some((field, clause)) = filter_obj.iter().next() else {
        step.insert("filter".to_string(), filter_value);
        return true;
    };
    let Some(clause_obj) = clause.as_object() else {
        step.insert("filter".to_string(), filter_value);
        return true;
    };
    if clause_obj.len() != 1 {
        step.insert("filter".to_string(), filter_value);
        return true;
    }

    let Some((operator, value)) = clause_obj.iter().next() else {
        step.insert("filter".to_string(), filter_value);
        return true;
    };

    step.entry("field".to_string())
        .or_insert_with(|| serde_json::json!(field));
    step.entry("operator".to_string())
        .or_insert_with(|| serde_json::json!(operator));
    step.entry("value".to_string())
        .or_insert_with(|| value.clone());
    true
}

fn normalize_plan_v2_value(mut value: serde_json::Value) -> serde_json::Value {
    let Some(root) = value.as_object_mut() else {
        return value;
    };
    let Some(steps) = root.get_mut("steps").and_then(|steps| steps.as_array_mut()) else {
        return value;
    };
    for step in &mut *steps {
        let Some(step_obj) = step.as_object_mut() else {
            continue;
        };
        normalize_filter_rows_step_value(step_obj);
    }
    steps.retain(|step| {
        !matches!(
            step.as_object()
                .and_then(|step_obj| step_obj.get("op"))
                .and_then(|op| op.as_str()),
            Some("output")
        )
    });
    value
}

fn parse_normalized_plan_v2(candidate: &str) -> Option<(PlanV2, serde_json::Value)> {
    let raw = serde_json::from_str::<serde_json::Value>(candidate).ok()?;
    let normalized = normalize_plan_v2_value(raw);
    let plan = serde_json::from_value::<PlanV2>(normalized.clone()).ok()?;
    Some((plan, normalized))
}

pub(crate) fn plan_v2_to_multistep(plan: &PlanV2) -> Option<MultiStepPlan> {
    if plan.steps.is_empty() {
        return None;
    }
    let mut saw_fetch = false;
    let mut steps = Vec::new();

    for step in &plan.steps {
        let step_id = if step.id.trim().is_empty() {
            return None;
        } else {
            step.id.clone()
        };
        match &step.op {
            PlanV2Op::Fetch {
                root_field,
                fields,
                first,
                offset,
                filter,
                order,
            } => {
                let ir = IRQuery {
                    root_field: root_field.clone(),
                    fields: fields.clone(),
                    first: *first,
                    offset: *offset,
                    filter: filter.clone(),
                    order: order.clone(),
                };
                let query = ir_to_graphql(&ir)?;
                saw_fetch = true;
                steps.push(ExecutableStep {
                    id: step_id,
                    description: format!("Fetch data in `{}` from `{}`.", step.id, root_field),
                    query: Some(query),
                    op: step.op.clone(),
                });
            }
            PlanV2Op::Aggregate {
                source,
                group_by,
                metrics,
            } => {
                let gb = if group_by.is_empty() {
                    "no grouping".to_string()
                } else {
                    format!("group by {}", group_by.join(", "))
                };
                let ms = if metrics.is_empty() {
                    "default metric aggregation".to_string()
                } else {
                    let metric_text = metrics
                        .iter()
                        .map(MetricSpec::to_string)
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("metrics {}", metric_text)
                };
                steps.push(ExecutableStep {
                    id: step_id.clone(),
                    description: format!(
                        "Aggregate in `{step_id}` from `{source}` with {gb} and {ms}."
                    ),
                    query: None,
                    op: step.op.clone(),
                });
            }
            PlanV2Op::Compare {
                left,
                right,
                metric,
            } => {
                let metric_text = metric
                    .as_ref()
                    .map_or("values".to_string(), |m| format!("metric `{m}`"));
                steps.push(ExecutableStep {
                    id: step_id.clone(),
                    description: format!(
                        "Compare in `{step_id}`: `{left}` vs `{right}` on {metric_text}."
                    ),
                    query: None,
                    op: step.op.clone(),
                });
            }
            PlanV2Op::FilterRows {
                source,
                field,
                operator,
                value,
            } => {
                steps.push(ExecutableStep {
                    id: step_id.clone(),
                    description: format!(
                        "Filter rows in `{step_id}` from `{source}` where `{field}` {} {}.",
                        operator, value
                    ),
                    query: None,
                    op: step.op.clone(),
                });
            }
            PlanV2Op::Rank {
                source,
                by,
                direction,
                limit,
            } => {
                let dir = direction.clone().unwrap_or_else(|| "desc".to_string());
                let lim = limit.map_or("all results".to_string(), |n| format!("top {n}"));
                steps.push(ExecutableStep {
                    id: step_id.clone(),
                    description: format!(
                        "Rank in `{step_id}` from `{source}` by `{by}` ({dir}), keeping {lim}."
                    ),
                    query: None,
                    op: step.op.clone(),
                });
            }
            PlanV2Op::DistanceHaversine {
                vessels_source,
                target_source,
            } => {
                steps.push(ExecutableStep {
                    id: step_id.clone(),
                    description: format!(
                        "Compute Haversine in `{step_id}` between `{vessels_source}` and `{target_source}`."
                    ),
                    query: None,
                    op: step.op.clone(),
                });
            }
            PlanV2Op::JoinOnTime {
                left,
                right,
                left_time_field,
                right_time_field,
                window_minutes,
            } => {
                let left_tf = left_time_field
                    .as_ref()
                    .map_or("auto".to_string(), std::clone::Clone::clone);
                let right_tf = right_time_field
                    .as_ref()
                    .map_or("auto".to_string(), std::clone::Clone::clone);
                let win = window_minutes.unwrap_or(0);
                steps.push(ExecutableStep {
                    id: step_id.clone(),
                    description: format!(
                        "Join on time in `{step_id}`: `{left}` + `{right}` (left_time `{left_tf}`, right_time `{right_tf}`, window {win}m)."
                    ),
                    query: None,
                    op: step.op.clone(),
                });
            }
            PlanV2Op::ThresholdCheck {
                source,
                field,
                operator,
                value,
            } => {
                steps.push(ExecutableStep {
                    id: step_id.clone(),
                    description: format!(
                        "Threshold check in `{step_id}` from `{source}`: `{field}` {operator} {value}."
                    ),
                    query: None,
                    op: step.op.clone(),
                });
            }
            PlanV2Op::TrendSummary {
                source,
                time_field,
                value_field,
            } => {
                steps.push(ExecutableStep {
                    id: step_id.clone(),
                    description: format!(
                        "Summarize trend in `{step_id}` from `{source}` over time field `{time_field}` using value field `{value_field}`."
                    ),
                    query: None,
                    op: step.op.clone(),
                });
            }
        }
    }

    if !saw_fetch {
        return None;
    }

    let rewrites = if plan.rewrites.is_empty() {
        "plan_v2".to_string()
    } else {
        plan.rewrites.join(", ")
    };
    let notes = if plan.notes.is_empty() {
        "PlanV2 parsed and compiled into executable fetch queries; post-fetch operations may be client-side."
            .to_string()
    } else {
        plan.notes.join(" | ")
    };

    Some(MultiStepPlan {
        rewrites,
        notes,
        steps,
        execute_error:
            "PlanV2 contains operations that may require client-side post-processing in execute mode."
                .to_string(),
    })
}

pub(crate) fn parse_plan_v2_struct_from_response(response: &str) -> Option<PlanV2> {
    for candidate in extract_json_candidates(response) {
        if let Some((plan, _)) = parse_normalized_plan_v2(&candidate) {
            return Some(plan);
        }
    }
    None
}

pub(crate) fn extract_plan_v2_json_from_response(response: &str) -> Option<String> {
    extract_json_candidates(response)
        .into_iter()
        .find_map(|candidate| {
            parse_normalized_plan_v2(&candidate)
                .and_then(|(_, normalized)| serde_json::to_string_pretty(&normalized).ok())
        })
}

fn json_has_unresolved_placeholder(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(s) => s.contains("${"),
        serde_json::Value::Array(items) => items.iter().any(json_has_unresolved_placeholder),
        serde_json::Value::Object(map) => map.values().any(json_has_unresolved_placeholder),
        _ => false,
    }
}

fn validate_order_shape(
    schema_registry: &SchemaRegistry,
    root_field: &str,
    order_value: &serde_json::Value,
) -> Result<(), String> {
    let map = order_value
        .as_object()
        .ok_or_else(|| "order must be an object with a single direction (asc/desc)".to_string())?;
    if map.is_empty() {
        return Err("order cannot be empty".to_string());
    }
    let Some(order_input) = schema_registry.query_order_input(root_field) else {
        return Err(format!(
            "root field '{}' does not support an order argument",
            root_field
        ));
    };
    let Some(order_fields) = schema_registry.input_field_names(order_input) else {
        return Ok(());
    };
    let field_names = order_fields
        .iter()
        .map(|f| f.to_ascii_lowercase())
        .collect::<std::collections::HashSet<_>>();
    if field_names.iter().all(|f| f == "asc" || f == "desc") {
        let has_asc = map.keys().any(|k| k.eq_ignore_ascii_case("asc"));
        let has_desc = map.keys().any(|k| k.eq_ignore_ascii_case("desc"));
        if has_asc == has_desc {
            return Err("order must specify exactly one of asc or desc".to_string());
        }
    }
    Ok(())
}

fn list_contains_operator_tokens(values: &[String]) -> Option<String> {
    for value in values {
        let lower = value.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
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
                | "from"
                | "to"
        ) {
            return Some(value.clone());
        }
    }
    None
}

fn detect_suspicious_in_list(
    schema_registry: &SchemaRegistry,
    root_field: &str,
    value: &serde_json::Value,
    path: &str,
) -> Option<String> {
    let root_filter_fields = schema_registry
        .root_filter_fields(root_field)
        .into_iter()
        .map(|field| field.to_ascii_lowercase())
        .collect::<HashSet<_>>();
    detect_suspicious_in_list_with_fields(value, path, &root_filter_fields)
}

#[allow(clippy::only_used_in_recursion)]
fn detect_suspicious_in_list_with_fields(
    value: &serde_json::Value,
    path: &str,
    root_filter_fields: &HashSet<String>,
) -> Option<String> {
    match value {
        serde_json::Value::Object(map) => {
            for (key, val) in map {
                let key_lower = key.to_ascii_lowercase();
                if matches!(key_lower.as_str(), "and" | "or" | "not") {
                    if let Some(issue) =
                        detect_suspicious_in_list_with_fields(val, path, root_filter_fields)
                    {
                        return Some(issue);
                    }
                    continue;
                }
                match val {
                    serde_json::Value::Object(op_map) => {
                        for (op, op_val) in op_map {
                            if op.eq_ignore_ascii_case("in") {
                                let values = op_val
                                    .as_array()
                                    .map(|arr| {
                                        arr.iter()
                                            .filter_map(|item| item.as_str().map(|s| s.to_string()))
                                            .collect::<Vec<_>>()
                                    })
                                    .unwrap_or_default();
                                if let Some(token) = list_contains_operator_tokens(&values) {
                                    return Some(format!(
                                        "filter `{}` uses `in` list containing operator token `{}`; use the operator key instead of embedding it in the list",
                                        key, token
                                    ));
                                }
                                if values
                                    .iter()
                                    .any(|v| root_filter_fields.contains(&v.to_ascii_lowercase()))
                                {
                                    return Some(format!(
                                        "filter `{}` uses `in` list containing filter field names; use values only",
                                        key
                                    ));
                                }
                            } else if let Some(issue) = detect_suspicious_in_list_with_fields(
                                op_val,
                                key,
                                root_filter_fields,
                            ) {
                                return Some(issue);
                            }
                        }
                    }
                    serde_json::Value::Array(items) => {
                        for item in items {
                            if let Some(issue) =
                                detect_suspicious_in_list_with_fields(item, key, root_filter_fields)
                            {
                                return Some(issue);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                if let Some(issue) =
                    detect_suspicious_in_list_with_fields(item, path, root_filter_fields)
                {
                    return Some(issue);
                }
            }
        }
        _ => {}
    }
    None
}

pub(crate) fn validate_plan_v2(
    plan: &PlanV2,
    schema_registry: &SchemaRegistry,
) -> PipelineResult<()> {
    if plan.version.as_deref() != Some("v2") {
        return Err(PipelineError::planning("plan version must be \"v2\""));
    }
    if plan.steps.is_empty() {
        return Err(PipelineError::planning(
            "plan must contain at least one step",
        ));
    }

    let mut ids = std::collections::HashSet::new();
    for step in &plan.steps {
        let step_id = step.id.trim();
        if step_id.is_empty() {
            return Err(PipelineError::planning(
                "each step must have a non-empty id",
            ));
        }
        if !ids.insert(step_id.to_string()) {
            return Err(PipelineError::planning(format!(
                "duplicate step id '{step_id}'"
            )));
        }
    }

    for step in &plan.steps {
        let sid = step.id.as_str();
        match &step.op {
            PlanV2Op::Fetch {
                root_field,
                fields,
                first,
                offset,
                filter,
                order,
            } => {
                if fields.is_empty() {
                    return Err(PipelineError::planning(format!(
                        "step '{sid}': fetch fields cannot be empty"
                    )));
                }
                for f in fields {
                    let trimmed = f.trim();
                    if trimmed.is_empty() {
                        return Err(PipelineError::planning(format!(
                            "step '{sid}': empty field path"
                        )));
                    }
                    if trimmed.contains('(') || trimmed.contains(')') || trimmed.contains(':') {
                        return Err(PipelineError::planning(format!(
                            "step '{sid}': invalid field syntax '{trimmed}'"
                        )));
                    }
                    if trimmed.contains("${") {
                        return Err(PipelineError::planning(format!(
                            "step '{sid}': unresolved placeholder in field '{trimmed}'"
                        )));
                    }
                }
                if let Some(v) = first
                    && *v < 0
                {
                    return Err(PipelineError::planning(format!(
                        "step '{sid}': first cannot be negative"
                    )));
                }
                if let Some(v) = offset
                    && *v < 0
                {
                    return Err(PipelineError::planning(format!(
                        "step '{sid}': offset cannot be negative"
                    )));
                }
                schema_registry
                    .validate_fetch_step(root_field, fields, filter.as_ref(), order.as_ref())
                    .map_err(|e| PipelineError::planning(format!("step '{sid}': {e}")))?;
                if let Some(filter_value) = filter
                    && let Some(issue) =
                        detect_suspicious_in_list(schema_registry, root_field, filter_value, "")
                {
                    return Err(PipelineError::planning(format!("step '{sid}': {issue}")));
                }
                if let Some(o) = order {
                    validate_order_shape(schema_registry, root_field, o)
                        .map_err(|e| PipelineError::planning(format!("step '{sid}': {e}")))?;
                }
            }
            PlanV2Op::Aggregate {
                source,
                group_by: _,
                metrics,
            } => {
                if source.trim().is_empty() {
                    return Err(PipelineError::planning(format!(
                        "step '{sid}': aggregate source is empty"
                    )));
                }
                if !ids.contains(source) {
                    return Err(PipelineError::planning(format!(
                        "step '{sid}': aggregate source '{source}' not found"
                    )));
                }
                if metrics.is_empty() {
                    return Err(PipelineError::planning(format!(
                        "step '{sid}': aggregate metrics cannot be empty"
                    )));
                }
                for metric in metrics {
                    if metric.requires_field()
                        && metric
                            .field()
                            .map(str::trim)
                            .filter(|v| !v.is_empty())
                            .is_none()
                    {
                        return Err(PipelineError::planning(format!(
                            "step '{sid}': aggregate metric `{metric}` requires a field"
                        )));
                    }
                    if let MetricSpec::Formula { name, expr } = metric {
                        if name.trim().is_empty() {
                            return Err(PipelineError::planning(format!(
                                "step '{sid}': formula metric requires a name"
                            )));
                        }
                        if let Err(e) = parse_metric_formula(expr) {
                            return Err(PipelineError::planning(format!(
                                "step '{sid}': invalid formula metric `{name}`: {e}"
                            )));
                        }
                    }
                }
            }
            PlanV2Op::Compare {
                left,
                right,
                metric,
            } => {
                if left.trim().is_empty() || right.trim().is_empty() {
                    return Err(PipelineError::planning(format!(
                        "step '{sid}': compare requires non-empty left/right"
                    )));
                }
                let Some(metric) = metric else {
                    return Err(PipelineError::planning(format!(
                        "step '{sid}': compare requires a metric"
                    )));
                };
                if metric.requires_field()
                    && metric
                        .field()
                        .map(str::trim)
                        .filter(|v| !v.is_empty())
                        .is_none()
                {
                    return Err(PipelineError::planning(format!(
                        "step '{sid}': compare metric `{metric}` requires a field"
                    )));
                }
                if let MetricSpec::Formula { name, expr } = metric {
                    if name.trim().is_empty() {
                        return Err(PipelineError::planning(format!(
                            "step '{sid}': formula metric requires a name"
                        )));
                    }
                    if let Err(e) = parse_metric_formula(expr) {
                        return Err(PipelineError::planning(format!(
                            "step '{sid}': invalid formula metric `{name}`: {e}"
                        )));
                    }
                }
                if !ids.contains(left) {
                    return Err(PipelineError::planning(format!(
                        "step '{sid}': compare left source '{left}' not found"
                    )));
                }
                if !ids.contains(right) {
                    return Err(PipelineError::planning(format!(
                        "step '{sid}': compare right source '{right}' not found"
                    )));
                }
            }
            PlanV2Op::FilterRows {
                source,
                field,
                operator,
                value,
            } => {
                if source.trim().is_empty() {
                    return Err(PipelineError::planning(format!(
                        "step '{sid}': filter_rows source is empty"
                    )));
                }
                if !ids.contains(source) {
                    return Err(PipelineError::planning(format!(
                        "step '{sid}': filter_rows source '{source}' not found"
                    )));
                }
                if field.trim().is_empty() {
                    return Err(PipelineError::planning(format!(
                        "step '{sid}': filter_rows field cannot be empty"
                    )));
                }
                let op = operator.trim().to_ascii_lowercase();
                let allowed = ["eq", "ne", "contains", "gt", "gte", "lt", "lte"];
                if !allowed.iter().any(|v| *v == op) {
                    return Err(PipelineError::planning(format!(
                        "step '{sid}': unsupported filter_rows operator '{operator}'"
                    )));
                }
                if json_has_unresolved_placeholder(value) {
                    return Err(PipelineError::planning(format!(
                        "step '{sid}': unresolved placeholder in filter_rows value"
                    )));
                }
            }
            PlanV2Op::Rank {
                source,
                by,
                direction,
                limit,
            } => {
                if source.trim().is_empty() || by.trim().is_empty() {
                    return Err(PipelineError::planning(format!(
                        "step '{sid}': rank requires source and by"
                    )));
                }
                if !ids.contains(source) {
                    return Err(PipelineError::planning(format!(
                        "step '{sid}': rank source '{source}' not found"
                    )));
                }
                if let Some(dir) = direction
                    && dir != "asc"
                    && dir != "desc"
                {
                    return Err(PipelineError::planning(format!(
                        "step '{sid}': rank direction must be 'asc' or 'desc'"
                    )));
                }
                if let Some(v) = limit
                    && *v == 0
                {
                    return Err(PipelineError::planning(format!(
                        "step '{sid}': rank limit must be > 0"
                    )));
                }
            }
            PlanV2Op::DistanceHaversine {
                vessels_source,
                target_source,
            } => {
                if vessels_source.trim().is_empty() || target_source.trim().is_empty() {
                    return Err(PipelineError::planning(format!(
                        "step '{sid}': distance_haversine requires source ids"
                    )));
                }
                if !ids.contains(vessels_source) {
                    return Err(PipelineError::planning(format!(
                        "step '{sid}': vessels_source '{vessels_source}' not found"
                    )));
                }
                if !ids.contains(target_source) {
                    return Err(PipelineError::planning(format!(
                        "step '{sid}': target_source '{target_source}' not found"
                    )));
                }
            }
            PlanV2Op::JoinOnTime {
                left,
                right,
                left_time_field,
                right_time_field,
                window_minutes,
            } => {
                if left.trim().is_empty() || right.trim().is_empty() {
                    return Err(PipelineError::planning(format!(
                        "step '{sid}': join_on_time requires non-empty left/right"
                    )));
                }
                if !ids.contains(left) {
                    return Err(PipelineError::planning(format!(
                        "step '{sid}': join_on_time left source '{left}' not found"
                    )));
                }
                if !ids.contains(right) {
                    return Err(PipelineError::planning(format!(
                        "step '{sid}': join_on_time right source '{right}' not found"
                    )));
                }
                if let Some(f) = left_time_field
                    && f.trim().is_empty()
                {
                    return Err(PipelineError::planning(format!(
                        "step '{sid}': left_time_field cannot be empty"
                    )));
                }
                if let Some(f) = right_time_field
                    && f.trim().is_empty()
                {
                    return Err(PipelineError::planning(format!(
                        "step '{sid}': right_time_field cannot be empty"
                    )));
                }
                if let Some(w) = window_minutes
                    && *w < 0
                {
                    return Err(PipelineError::planning(format!(
                        "step '{sid}': window_minutes cannot be negative"
                    )));
                }
            }
            PlanV2Op::ThresholdCheck {
                source,
                field,
                operator,
                value,
            } => {
                if source.trim().is_empty() || field.trim().is_empty() {
                    return Err(PipelineError::planning(format!(
                        "step '{sid}': threshold_check requires non-empty source and field"
                    )));
                }
                if !ids.contains(source) {
                    return Err(PipelineError::planning(format!(
                        "step '{sid}': threshold_check source '{source}' not found"
                    )));
                }
                let op = operator.trim();
                if !matches!(op, ">" | ">=" | "<" | "<=" | "=" | "==" | "!=") {
                    return Err(PipelineError::planning(format!(
                        "step '{sid}': threshold_check operator must be one of >, >=, <, <=, =, ==, !="
                    )));
                }
                if !value.is_finite() {
                    return Err(PipelineError::planning(format!(
                        "step '{sid}': threshold_check value must be finite"
                    )));
                }
            }
            PlanV2Op::TrendSummary {
                source,
                time_field,
                value_field,
            } => {
                if source.trim().is_empty()
                    || time_field.trim().is_empty()
                    || value_field.trim().is_empty()
                {
                    return Err(PipelineError::planning(format!(
                        "step '{sid}': trend_summary requires non-empty source, time_field, and value_field"
                    )));
                }
                if !ids.contains(source) {
                    return Err(PipelineError::planning(format!(
                        "step '{sid}': trend_summary source '{source}' not found"
                    )));
                }
            }
        }
    }

    Ok(())
}

#[derive(Default)]
struct QueryScope {
    roots: std::collections::BTreeSet<String>,
    literals: std::collections::BTreeSet<String>,
}

fn collect_scope_literals_from_value(
    value: &QueryValue<'_, String>,
    out: &mut std::collections::BTreeSet<String>,
) {
    match value {
        QueryValue::String(s) | QueryValue::Enum(s) => {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                out.insert(trimmed.to_string());
            }
        }
        QueryValue::Int(n) => {
            if let Some(v) = n.as_i64() {
                out.insert(v.to_string());
            }
        }
        QueryValue::Float(n) => {
            out.insert(n.to_string());
        }
        QueryValue::List(items) => {
            for item in items {
                collect_scope_literals_from_value(item, out);
            }
        }
        QueryValue::Object(map) => {
            for item in map.values() {
                collect_scope_literals_from_value(item, out);
            }
        }
        _ => {}
    }
}

fn collect_query_scope(query_text: &str) -> QueryScope {
    fn walk_selection_set(
        selections: &[Selection<'_, String>],
        scope: &mut QueryScope,
        top_level: bool,
    ) {
        for selection in selections {
            match selection {
                Selection::Field(field) => {
                    if top_level {
                        scope.roots.insert(field.name.clone());
                    }
                    for (arg_name, value) in &field.arguments {
                        if arg_name == "first" || arg_name == "offset" || arg_name == "order" {
                            continue;
                        }
                        collect_scope_literals_from_value(value, &mut scope.literals);
                    }
                    walk_selection_set(&field.selection_set.items, scope, false);
                }
                Selection::InlineFragment(fragment) => {
                    walk_selection_set(&fragment.selection_set.items, scope, top_level);
                }
                Selection::FragmentSpread(_) => {}
            }
        }
    }

    let mut out = QueryScope::default();
    let Ok(doc) = parse_query::<String>(query_text) else {
        return out;
    };

    for def in doc.definitions {
        if let QueryDefinition::Operation(op) = def {
            match op {
                OperationDefinition::Query(q) => {
                    walk_selection_set(&q.selection_set.items, &mut out, true);
                }
                OperationDefinition::SelectionSet(set) => {
                    walk_selection_set(&set.items, &mut out, true);
                }
                OperationDefinition::Mutation(m) => {
                    walk_selection_set(&m.selection_set.items, &mut out, true);
                }
                OperationDefinition::Subscription(s) => {
                    walk_selection_set(&s.selection_set.items, &mut out, true);
                }
            }
        }
    }

    out
}

pub(crate) fn collect_query_scope_literals(query_text: &str) -> Vec<String> {
    collect_query_scope(query_text)
        .literals
        .into_iter()
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ScopeConstraint {
    root: String,
    field: String,
    op: String,
    values: Vec<String>,
}

fn normalize_operator(op: &str) -> String {
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
    let planned = normalize_operator(planned);
    let executed = normalize_operator(executed);
    planned.eq_ignore_ascii_case(&executed) || (planned == "eq" && executed == "in")
}

fn is_filter_operator_key(key: &str) -> bool {
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
            | "from"
            | "to"
    )
}

fn collect_scope_literal_values(value: &QueryValue<'_, String>, out: &mut Vec<String>) {
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
                collect_scope_literal_values(item, out);
            }
        }
        QueryValue::Object(map) => {
            for value in map.values() {
                collect_scope_literal_values(value, out);
            }
        }
        QueryValue::Boolean(b) => {
            out.push(b.to_string());
        }
        _ => {}
    }
}

fn add_scope_constraint(
    out: &mut Vec<ScopeConstraint>,
    root: &str,
    field: &str,
    op: &str,
    values: Vec<String>,
) {
    if values.is_empty() {
        return;
    }
    let op = normalize_operator(op);
    if let Some(existing) = out.iter_mut().find(|c| {
        c.root.eq_ignore_ascii_case(root)
            && c.field.eq_ignore_ascii_case(field)
            && c.op.eq_ignore_ascii_case(&op)
    }) {
        for value in values {
            if !existing
                .values
                .iter()
                .any(|v| v.eq_ignore_ascii_case(&value))
            {
                existing.values.push(value);
            }
        }
        return;
    }
    out.push(ScopeConstraint {
        root: root.to_string(),
        field: field.to_string(),
        op,
        values,
    });
}

fn collect_constraints_for_filter(
    root: &str,
    value: &QueryValue<'_, String>,
    path: String,
    out: &mut Vec<ScopeConstraint>,
) {
    match value {
        QueryValue::Object(map) => {
            for (key, value) in map {
                let key_lower = key.to_ascii_lowercase();
                if matches!(key_lower.as_str(), "and" | "or" | "not") {
                    collect_constraints_for_filter(root, value, path.clone(), out);
                    continue;
                }
                if is_filter_operator_key(&key_lower) && path.is_empty() {
                    continue;
                }
                if is_filter_operator_key(&key_lower) && !path.is_empty() {
                    let mut values = Vec::new();
                    collect_scope_literal_values(value, &mut values);
                    add_scope_constraint(out, root, &path, &key_lower, values);
                    continue;
                }
                let new_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{}.{}", path, key)
                };
                match value {
                    QueryValue::Object(_) | QueryValue::List(_) => {
                        collect_constraints_for_filter(root, value, new_path, out);
                    }
                    _ => {
                        let mut values = Vec::new();
                        collect_scope_literal_values(value, &mut values);
                        add_scope_constraint(out, root, &new_path, "eq", values);
                    }
                }
            }
        }
        QueryValue::List(items) => {
            for item in items {
                collect_constraints_for_filter(root, item, path.clone(), out);
            }
        }
        _ => {
            if path.is_empty() {
                return;
            }
            let mut values = Vec::new();
            collect_scope_literal_values(value, &mut values);
            add_scope_constraint(out, root, &path, "eq", values);
        }
    }
}

fn collect_scope_constraints(query_text: &str) -> Vec<ScopeConstraint> {
    let Ok(doc) = parse_query::<String>(query_text) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for def in doc.definitions {
        let selection_set = match def {
            QueryDefinition::Operation(OperationDefinition::Query(q)) => q.selection_set,
            QueryDefinition::Operation(OperationDefinition::SelectionSet(set)) => set,
            QueryDefinition::Operation(OperationDefinition::Mutation(m)) => m.selection_set,
            QueryDefinition::Operation(OperationDefinition::Subscription(s)) => s.selection_set,
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
                collect_constraints_for_filter(&root, filter, String::new(), &mut out);
            }
        }
    }
    out
}

fn resolve_case_insensitive_field(fields: &[String], candidate: &str) -> Option<String> {
    fields
        .iter()
        .find(|field| field.eq_ignore_ascii_case(candidate))
        .cloned()
}

fn regex_alternation_pattern(values: &[String]) -> Option<String> {
    let mut ordered = values
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    ordered.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    ordered.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    if ordered.is_empty() {
        return None;
    }
    Some(
        ordered
            .into_iter()
            .map(regex::escape)
            .collect::<Vec<_>>()
            .join("|"),
    )
}

fn normalized_phrase_tokens(value: &str) -> Vec<String> {
    value
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

fn vocabulary_term_matches(raw: &str, terms: &[String]) -> bool {
    let raw_tokens = normalized_phrase_tokens(raw);
    !raw_tokens.is_empty()
        && terms
            .iter()
            .map(|term| normalized_phrase_tokens(term))
            .any(|term_tokens| !term_tokens.is_empty() && term_tokens == raw_tokens)
}

fn vocabulary_term_prefixes_value(value: &str, terms: &[String]) -> bool {
    let value_tokens = normalized_phrase_tokens(value);
    !value_tokens.is_empty()
        && terms
            .iter()
            .map(|term| normalized_phrase_tokens(term))
            .any(|term_tokens| {
                !term_tokens.is_empty()
                    && value_tokens.len() > term_tokens.len()
                    && value_tokens[..term_tokens.len()] == term_tokens
            })
}

fn filter_operator_for_vocabulary_term(
    raw_operator: &str,
    vocabulary: &IntentVocabulary,
) -> Option<String> {
    if vocabulary_term_matches(raw_operator, &vocabulary.filter_contains) {
        return Some("contains".to_string());
    }
    if vocabulary_term_matches(raw_operator, &vocabulary.filter_eq) {
        return Some("eq".to_string());
    }
    None
}

fn extract_explicit_field_constraint_parts(
    user_message: &str,
    vocabulary: &IntentVocabulary,
) -> Option<(String, String, String)> {
    let operator_terms = vocabulary
        .filter_contains
        .iter()
        .chain(vocabulary.filter_eq.iter())
        .cloned()
        .collect::<Vec<_>>();
    let operator_pattern = regex_alternation_pattern(&operator_terms)?;
    let re = Regex::new(&format!(
        r#"(?i)\b([A-Za-z_][A-Za-z0-9_]*)\b\s+({operator_pattern})\s+['"]([^'"]+)['"]"#
    ))
    .ok()?;
    let caps = re.captures(user_message)?;
    let raw_field = caps.get(1)?.as_str().to_string();
    let raw_operator = caps.get(2)?.as_str();
    let value = caps.get(3)?.as_str().to_string();
    let operator = filter_operator_for_vocabulary_term(raw_operator, vocabulary)?;
    Some((raw_field, operator, value))
}

fn extract_explicit_field_constraint(
    schema_registry: &SchemaRegistry,
    root_field: &str,
    user_message: &str,
    vocabulary: &IntentVocabulary,
) -> Option<(String, String, String)> {
    let filter_fields = schema_registry.root_filter_fields(root_field);
    if filter_fields.is_empty() {
        return None;
    }
    let (raw_field, operator, value) =
        extract_explicit_field_constraint_parts(user_message, vocabulary)?;
    let field = resolve_case_insensitive_field(&filter_fields, &raw_field)?;
    Some((field, operator, value))
}

fn root_scalar_fields(schema_registry: &SchemaRegistry, root_field: &str) -> Vec<String> {
    let Some(type_name) = schema_registry.query_return_type(root_field) else {
        return Vec::new();
    };
    let Some(field_names) = schema_registry.object_field_names(type_name) else {
        return Vec::new();
    };
    field_names
        .iter()
        .filter(|field| {
            schema_registry
                .object_field_type(type_name, field)
                .is_some_and(|field_type| schema_registry.object_field_names(field_type).is_none())
        })
        .cloned()
        .collect()
}

fn extract_field_phrase_after_grouping_term(
    user_message: &str,
    vocabulary: &IntentVocabulary,
) -> Option<String> {
    let group_pattern = regex_alternation_pattern(&vocabulary.group_by)?;
    let re = Regex::new(&format!(
        r"(?i)\b(?:{group_pattern})\s+([A-Za-z_][A-Za-z0-9_]*)"
    ))
    .ok()?;
    let caps = re.captures(user_message)?;
    Some(caps.get(1)?.as_str().to_string())
}

fn candidate_is_meaningful_scope_value(
    candidate: &str,
    vocabulary: Option<&IntentVocabulary>,
) -> bool {
    let trimmed = candidate.trim();
    if trimmed.is_empty() {
        return false;
    }
    if let Some(vocabulary) = vocabulary
        && (vocabulary_term_prefixes_value(trimmed, &vocabulary.rank_desc)
            || vocabulary_term_prefixes_value(trimmed, &vocabulary.rank_asc))
    {
        return false;
    }
    if trimmed.chars().all(|ch| ch.is_ascii_digit()) {
        return false;
    }
    true
}

fn select_fallback_root_field(
    schema_registry: &SchemaRegistry,
    user_message: &str,
    sls: Option<&Sls>,
) -> Option<String> {
    let vocabulary = sls.map(|sls| &sls.intent_vocabulary);
    let root_candidates = schema_registry.best_matching_query_roots(user_message, 5);
    if root_candidates.is_empty() {
        return None;
    }
    let compact_identifier_candidates = extract_identifier_candidates(user_message)
        .into_iter()
        .filter(|candidate| {
            candidate_is_meaningful_scope_value(candidate, vocabulary)
                && looks_like_compact_identifier_scope(candidate)
        })
        .collect::<Vec<_>>();
    if !compact_identifier_candidates.is_empty() {
        let identifier_roots = root_candidates
            .iter()
            .filter(|root| {
                !schema_registry
                    .root_identifier_filter_fields(root)
                    .is_empty()
            })
            .cloned()
            .collect::<Vec<_>>();
        return identifier_roots.into_iter().next();
    }
    if let Some((raw_field, _, _)) = vocabulary
        .and_then(|vocabulary| extract_explicit_field_constraint_parts(user_message, vocabulary))
    {
        let matching_roots = root_candidates
            .iter()
            .filter(|root| {
                resolve_case_insensitive_field(
                    &schema_registry.root_filter_fields(root),
                    &raw_field,
                )
                .is_some()
            })
            .cloned()
            .collect::<Vec<_>>();
        return matching_roots.into_iter().next();
    }
    let metric_hint = vocabulary
        .and_then(|vocabulary| extract_field_phrase_after_grouping_term(user_message, vocabulary));
    if let Some(metric_hint) = metric_hint {
        let matching_roots = root_candidates
            .iter()
            .filter(|root| {
                let scalar_fields = root_scalar_fields(schema_registry, root);
                resolve_case_insensitive_field(&scalar_fields, &metric_hint).is_some()
            })
            .cloned()
            .collect::<Vec<_>>();
        return matching_roots.into_iter().next();
    }
    None
}

pub(crate) fn extract_identifier_candidates(user_message: &str) -> Vec<String> {
    let mut out = Vec::new();
    fn push_candidate(out: &mut Vec<String>, raw: &str) {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return;
        }
        let mut tokens = trimmed
            .split_whitespace()
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .collect::<Vec<_>>();
        if tokens.is_empty() {
            return;
        }
        if tokens.len() > 1
            && let Some(first_upper_idx) = tokens.iter().position(|token| {
                token
                    .chars()
                    .next()
                    .is_some_and(|ch| ch.is_ascii_uppercase())
            })
            && first_upper_idx > 0
        {
            let has_mixed_case = tokens
                .iter()
                .any(|token| token.chars().any(|ch| ch.is_ascii_uppercase()))
                && tokens
                    .iter()
                    .any(|token| token.chars().any(|ch| ch.is_ascii_lowercase()));
            if has_mixed_case {
                tokens = tokens.into_iter().skip(first_upper_idx).collect::<Vec<_>>();
            }
        }
        let normalized = tokens.join(" ");
        if normalized.is_empty() {
            return;
        }
        if !out
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(&normalized))
        {
            out.push(normalized);
        }
    }
    if let Ok(quoted) = Regex::new(r#"['"]([^'"]+)['"]"#) {
        for caps in quoted.captures_iter(user_message) {
            if let Some(value) = caps.get(1).map(|m| m.as_str().trim()) {
                push_candidate(&mut out, value);
            }
        }
    }
    if let Ok(tokens) = Regex::new(r"\b[A-Za-z0-9_-]*\d+[A-Za-z0-9_-]*\b") {
        for caps in tokens.captures_iter(user_message) {
            if let Some(value) = caps.get(0).map(|m| m.as_str().trim()) {
                push_candidate(&mut out, value);
            }
        }
    }
    out
}

fn message_mentions_location_concept(user_message: &str, sls: Option<&Sls>) -> bool {
    sls.is_some_and(|s| s.message_mentions_concept(user_message, "location"))
}

fn looks_like_compact_identifier_scope(candidate: &str) -> bool {
    let trimmed = candidate.trim();
    if trimmed.is_empty() || trimmed.contains(char::is_whitespace) {
        return false;
    }
    let has_digit = trimmed.chars().any(|c| c.is_ascii_digit());
    let has_sep = trimmed.contains('-') || trimmed.contains('_') || trimmed.contains(':');
    let has_alpha = trimmed.chars().any(|c| c.is_ascii_alphabetic());
    has_alpha && (has_digit || has_sep)
}

#[allow(clippy::collapsible_if)]
fn typed_json_value(type_name: &str, raw: &str) -> serde_json::Value {
    let lower = type_name.to_ascii_lowercase();
    if lower == "int" || lower.ends_with("int") {
        if let Ok(v) = raw.parse::<i64>() {
            return serde_json::json!(v);
        }
    }
    if lower == "float"
        || lower.contains("float")
        || lower.contains("double")
        || lower.contains("decimal")
        || lower.contains("number")
    {
        if let Ok(v) = raw.parse::<f64>() {
            return serde_json::json!(v);
        }
    }
    if lower == "boolean" || lower == "bool" {
        if let Ok(v) = raw.parse::<bool>() {
            return serde_json::json!(v);
        }
    }
    serde_json::json!(raw)
}

fn build_filter_clause_for_values(
    schema_registry: &SchemaRegistry,
    root_field: &str,
    field: &str,
    values: &[String],
    operator_hint: Option<&str>,
) -> Option<serde_json::Value> {
    if values.is_empty() {
        return None;
    }
    let mut unique_values: Vec<String> = Vec::new();
    for value in values {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !unique_values
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(trimmed))
        {
            unique_values.push(trimmed.to_string());
        }
    }
    if unique_values.is_empty() {
        return None;
    }
    let type_ref = schema_registry.filter_field_type_ref(root_field, field)?;
    if let Some(op_fields) = schema_registry.input_field_names(&type_ref.name) {
        let ops = op_fields
            .iter()
            .map(|op| op.to_ascii_lowercase())
            .collect::<HashSet<_>>();
        let op = match operator_hint {
            Some(hint) if hint.eq_ignore_ascii_case("contains") => {
                if ops.contains("contains") {
                    "contains"
                } else if ops.contains("like") {
                    "like"
                } else {
                    return None;
                }
            }
            _ => {
                if unique_values.len() > 1 && ops.contains("in") {
                    "in"
                } else if ops.contains("eq") {
                    "eq"
                } else if ops.contains("in") {
                    "in"
                } else {
                    return None;
                }
            }
        };
        let op_type = schema_registry.input_field_type_ref(&type_ref.name, op)?;
        let value_json = if op_type.is_list {
            let items = unique_values
                .iter()
                .map(|value| typed_json_value(&op_type.name, value))
                .collect::<Vec<_>>();
            serde_json::Value::Array(items)
        } else {
            typed_json_value(&op_type.name, unique_values.first()?)
        };
        let mut op_map = serde_json::Map::new();
        op_map.insert(op.to_string(), value_json);
        let mut field_map = serde_json::Map::new();
        field_map.insert(field.to_string(), serde_json::Value::Object(op_map));
        return Some(serde_json::Value::Object(field_map));
    }
    let value_json = if type_ref.is_list {
        let items = unique_values
            .iter()
            .map(|value| typed_json_value(&type_ref.name, value))
            .collect::<Vec<_>>();
        serde_json::Value::Array(items)
    } else {
        typed_json_value(&type_ref.name, unique_values.first()?)
    };
    let mut field_map = serde_json::Map::new();
    field_map.insert(field.to_string(), value_json);
    Some(serde_json::Value::Object(field_map))
}

pub(crate) fn synthesize_simple_fetch_plan(
    schema_registry: &SchemaRegistry,
    user_message: &str,
    sls: Option<&Sls>,
) -> Option<PlanV2> {
    fn append_lookup_label_fields(
        schema_registry: &SchemaRegistry,
        root_field: &str,
        fields: &mut Vec<String>,
    ) {
        let Some(type_name) = schema_registry.query_return_type(root_field) else {
            return;
        };
        let Some(object_fields) = schema_registry.object_field_names(type_name) else {
            return;
        };
        for label in schema_registry
            .field_roles_for_root(root_field)
            .label_fields
        {
            if object_fields.contains(&label) && !fields.iter().any(|field| field == &label) {
                fields.push(label);
            }
        }
    }

    let explicit_field_constraint_parts = sls.and_then(|sls| {
        extract_explicit_field_constraint_parts(user_message, &sls.intent_vocabulary)
    });
    if sls
        .and_then(|sls| sls.simple_fetch_fallback_denied_intent(user_message))
        .is_some_and(|intent| intent != "membership" || explicit_field_constraint_parts.is_none())
    {
        return None;
    }

    let simple_fetch_policy = sls.and_then(Sls::simple_fetch_fallback_policy);
    let root_field = select_fallback_root_field(schema_registry, user_message, sls)?;
    let mut fields = schema_registry.default_scalar_fields_for_root(&root_field, 12);
    if fields.is_empty() {
        return None;
    }

    let mut filter = None;
    let mut extra_steps = Vec::new();

    if let Some((field, operator, value)) = sls.and_then(|sls| {
        extract_explicit_field_constraint(
            schema_registry,
            &root_field,
            user_message,
            &sls.intent_vocabulary,
        )
    }) {
        if simple_fetch_policy.is_some_and(|policy| !policy.allow_explicit_field_constraints) {
            return None;
        }
        if operator != "contains" {
            append_lookup_label_fields(schema_registry, &root_field, &mut fields);
        }
        if !fields.iter().any(|f| f == &field) {
            fields.push(field.clone());
        }
        if operator == "contains" {
            if let Some(clause) = build_filter_clause_for_values(
                schema_registry,
                &root_field,
                &field,
                std::slice::from_ref(&value),
                Some("contains"),
            ) {
                filter = Some(clause);
            } else {
                extra_steps.push(PlanV2Step {
                    id: "s2".to_string(),
                    op: PlanV2Op::FilterRows {
                        source: "s1".to_string(),
                        field,
                        operator,
                        value: serde_json::json!(value),
                    },
                });
            }
        } else if let Some(clause) = build_filter_clause_for_values(
            schema_registry,
            &root_field,
            &field,
            std::slice::from_ref(&value),
            Some("eq"),
        ) {
            filter = Some(clause);
        } else {
            extra_steps.push(PlanV2Step {
                id: "s2".to_string(),
                op: PlanV2Op::FilterRows {
                    source: "s1".to_string(),
                    field,
                    operator,
                    value: serde_json::json!(value),
                },
            });
        }
    } else {
        let candidates = extract_identifier_candidates(user_message)
            .into_iter()
            .filter(|candidate| {
                candidate_is_meaningful_scope_value(
                    candidate,
                    sls.map(|sls| &sls.intent_vocabulary),
                ) && looks_like_compact_identifier_scope(candidate)
            })
            .collect::<Vec<_>>();
        if !candidates.is_empty() {
            if simple_fetch_policy.is_some_and(|policy| !policy.allow_compact_identifier_lookup) {
                return None;
            }
            let id_fields = schema_registry.root_identifier_filter_fields(&root_field);
            if !id_fields.is_empty() {
                let mut clauses = Vec::new();
                for field in id_fields {
                    if let Some(clause) = build_filter_clause_for_values(
                        schema_registry,
                        &root_field,
                        &field,
                        &candidates,
                        Some("eq"),
                    ) {
                        clauses.push(clause);
                    }
                }
                if !clauses.is_empty() {
                    filter = Some(serde_json::json!({ "or": clauses }));
                }
            }
        }
    }

    if filter.is_none() && extra_steps.is_empty() {
        return None;
    }

    let rewrites = vec!["deterministic_simple_fetch_fallback".to_string()];
    let notes = vec![
        "Planner fallback synthesized a schema-aware simple fetch plan after PlanV2 parse/repair failure."
            .to_string(),
    ];

    let mut steps = vec![PlanV2Step {
        id: "s1".to_string(),
        op: PlanV2Op::Fetch {
            root_field,
            fields,
            first: None,
            offset: None,
            filter,
            order: None,
        },
    }];
    steps.extend(extra_steps);

    Some(PlanV2 {
        version: Some("v2".to_string()),
        rewrites,
        notes,
        steps,
    })
}

fn collect_filter_string_constraints(value: &serde_json::Value, out: &mut Vec<(String, String)>) {
    if let serde_json::Value::Object(map) = value {
        for (key, val) in map {
            let key_lower = key.to_ascii_lowercase();
            if key_lower == "and" || key_lower == "or" {
                if let serde_json::Value::Array(items) = val {
                    for item in items {
                        collect_filter_string_constraints(item, out);
                    }
                }
                continue;
            }
            if key_lower == "not" {
                collect_filter_string_constraints(val, out);
                continue;
            }
            match val {
                serde_json::Value::String(s) if !s.trim().is_empty() => {
                    out.push((key.clone(), s.clone()));
                }
                serde_json::Value::String(_) => {}
                serde_json::Value::Object(op_map) => {
                    for op_val in op_map.values() {
                        match op_val {
                            serde_json::Value::String(s) if !s.trim().is_empty() => {
                                out.push((key.clone(), s.clone()));
                            }
                            serde_json::Value::String(_) => {}
                            serde_json::Value::Array(items) => {
                                for item in items {
                                    if let serde_json::Value::String(s) = item
                                        && !s.trim().is_empty()
                                    {
                                        out.push((key.clone(), s.clone()));
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

fn is_label_like_value(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return false;
    }
    let has_alpha = trimmed.chars().any(|c| c.is_ascii_alphabetic());
    if !has_alpha {
        return false;
    }
    let has_space = trimmed.chars().any(|c| c.is_whitespace());
    let has_lower = trimmed.chars().any(|c| c.is_ascii_lowercase());
    let id_like = !has_space
        && !has_lower
        && trimmed
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | ':'));
    !id_like
}

fn placeholder_source_field(value: &str) -> Option<(&str, &str)> {
    let trimmed = value.trim();
    let inner = trimmed.strip_prefix("${")?.strip_suffix('}')?;
    let (source, field) = inner.split_once('.')?;
    let source = source.trim();
    let field = field.trim();
    if source.is_empty() || field.is_empty() {
        return None;
    }
    Some((source, field))
}

fn relation_nested_fields(
    schema_registry: &SchemaRegistry,
    child_root: &str,
    relation_field: &str,
    child_fields: &[String],
) -> Vec<String> {
    let mut fields = child_fields
        .iter()
        .filter(|field| !field.trim().is_empty())
        .map(|field| {
            if field.starts_with(relation_field) {
                field.clone()
            } else {
                format!("{relation_field}.{field}")
            }
        })
        .collect::<Vec<_>>();
    if fields.is_empty() {
        fields = schema_registry
            .default_scalar_fields_for_root(child_root, 6)
            .into_iter()
            .map(|field| format!("{relation_field}.{field}"))
            .collect();
    }
    fields
}

type FetchStepSnapshot = (
    String,
    String,
    Vec<String>,
    Option<i64>,
    Option<i64>,
    Option<serde_json::Value>,
);

fn apply_two_step_parent_relation_rewrite(
    plan: &mut PlanV2,
    fetch_steps: &[FetchStepSnapshot],
    schema_registry: &SchemaRegistry,
    sls: Option<&Sls>,
) -> bool {
    let Some(sls) = sls else {
        return false;
    };
    if fetch_steps.len() != 2 {
        return false;
    }

    for child_idx in 0..fetch_steps.len() {
        let parent_idx = 1 - child_idx;
        let (child_id, child_root, child_fields, _child_first, _child_offset, child_filter) =
            &fetch_steps[child_idx];
        let (parent_id, parent_root, _parent_fields, parent_first, parent_offset, parent_filter) =
            &fetch_steps[parent_idx];
        let Some(child_filter) = child_filter else {
            continue;
        };
        let mut constraints = Vec::new();
        collect_filter_string_constraints(child_filter, &mut constraints);
        if !constraints.iter().any(|(_field, value)| {
            placeholder_source_field(value)
                .is_some_and(|(source, _field)| source.eq_ignore_ascii_case(parent_id))
        }) {
            continue;
        }

        let Some(parent_type) = schema_registry.query_return_type(parent_root) else {
            continue;
        };
        let Some(child_type) = schema_registry.query_return_type(child_root) else {
            continue;
        };
        let Some(join) = sls.preferred_join_for_pair(parent_root, child_root) else {
            continue;
        };
        if join.strategy.as_deref() != Some("parent_relation") {
            continue;
        }
        let relation_fields = schema_registry.object_fields_with_type(parent_type, child_type);
        let Some(relation_field) = relation_fields.first().cloned() else {
            continue;
        };

        let parent_roles = schema_registry.field_roles_for_root(parent_root);
        let mut seen = HashSet::new();
        let mut fields = Vec::new();
        for field in parent_roles.label_fields {
            if schema_registry
                .object_field_names(parent_type)
                .is_some_and(|object_fields| object_fields.contains(&field))
                && seen.insert(field.to_ascii_lowercase())
            {
                fields.push(field);
            }
        }
        for field in
            relation_nested_fields(schema_registry, child_root, &relation_field, child_fields)
        {
            if seen.insert(field.to_ascii_lowercase()) {
                fields.push(field);
            }
        }
        if fields.is_empty() {
            continue;
        }

        if !plan
            .rewrites
            .iter()
            .any(|rewrite| rewrite == "parent_relation_rewrite")
        {
            plan.rewrites.push("parent_relation_rewrite".to_string());
        }
        plan.notes.push(format!(
            "Rewrote placeholder child fetch `{child_id}` into parent relation `{relation_field}`."
        ));
        plan.steps = vec![PlanV2Step {
            id: parent_id.clone(),
            op: PlanV2Op::Fetch {
                root_field: parent_root.clone(),
                fields,
                first: *parent_first,
                offset: *parent_offset,
                filter: parent_filter.clone(),
                order: None,
            },
        }];
        return true;
    }

    false
}

fn message_mentions_membership_intent(user_message: &str, sls: Option<&Sls>) -> bool {
    sls.is_some_and(|sls| sls.message_mentions_intent(user_message, "membership"))
}

fn message_mentions_root_concept(sls: &Sls, user_message: &str, root_field: &str) -> bool {
    sls.concepts.iter().any(|(concept_key, concept)| {
        concept
            .canonical_path
            .as_deref()
            .is_some_and(|root| root.eq_ignore_ascii_case(root_field))
            && sls.message_mentions_concept(user_message, concept_key)
    })
}

fn sls_parent_relation_candidates(
    schema_registry: &SchemaRegistry,
    user_message: &str,
    sls: &Sls,
    child_root: &str,
    child_type: &str,
) -> Vec<(usize, String, String)> {
    let mut candidates: Vec<(usize, String, String)> = Vec::new();
    let mut push_candidate = |score: usize, parent_root: &str| {
        if parent_root.eq_ignore_ascii_case(child_root) {
            return;
        }
        if !message_mentions_root_concept(sls, user_message, parent_root) {
            return;
        }
        let Some(parent_type) = schema_registry.query_return_type(parent_root) else {
            return;
        };
        let relation_fields = schema_registry.object_fields_with_type(parent_type, child_type);
        let Some(relation_field) = relation_fields.first() else {
            return;
        };
        if candidates
            .iter()
            .any(|(_, existing_root, _)| existing_root.eq_ignore_ascii_case(parent_root))
        {
            return;
        }
        candidates.push((score, parent_root.to_string(), relation_field.clone()));
    };

    for (idx, join) in sls.preferred_join_paths.iter().enumerate() {
        if join.strategy.as_deref() != Some("parent_relation")
            || !join.to_root.eq_ignore_ascii_case(child_root)
        {
            continue;
        }
        push_candidate(10_000usize.saturating_sub(idx), &join.from_root);
    }

    for concept in sls.concepts.values() {
        let Some(parent_root) = concept.canonical_path.as_deref() else {
            continue;
        };
        push_candidate(1, parent_root);
    }

    candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    candidates
}

pub(crate) fn apply_parent_relation_rewrite(
    plan: &mut PlanV2,
    user_message: &str,
    schema_registry: &SchemaRegistry,
    sls: Option<&Sls>,
) -> bool {
    if plan.steps.is_empty() {
        return false;
    }
    let mut fetch_steps = Vec::new();
    let mut other_ops = false;
    let mut filter_rows_steps = Vec::new();

    for step in &plan.steps {
        match &step.op {
            PlanV2Op::Fetch {
                root_field,
                fields,
                first,
                offset,
                filter,
                order: _,
            } => {
                fetch_steps.push((
                    step.id.clone(),
                    root_field.clone(),
                    fields.clone(),
                    *first,
                    *offset,
                    filter.clone(),
                ));
            }
            PlanV2Op::FilterRows { .. } => filter_rows_steps.push(step.clone()),
            _ => {
                other_ops = true;
                break;
            }
        }
    }
    if other_ops || fetch_steps.len() != 1 {
        if !other_ops
            && filter_rows_steps.is_empty()
            && apply_two_step_parent_relation_rewrite(plan, &fetch_steps, schema_registry, sls)
        {
            return true;
        }
        return false;
    }
    let (fetch_id, root_field, _fetch_fields, first, offset, fetch_filter) =
        fetch_steps.into_iter().next().expect("single fetch step");

    let child_type = schema_registry.query_return_type(&root_field);
    let Some(child_type) = child_type else {
        return false;
    };
    let Some(sls) = sls else {
        return false;
    };
    if !message_mentions_membership_intent(user_message, Some(sls)) {
        return false;
    }
    if message_mentions_location_concept(user_message, Some(sls))
        && let Some(concept) = sls.concepts.get("location")
        && let Some(id_fields) = &concept.id_fields
    {
        let filter_fields = schema_registry.root_filter_fields(&root_field);
        let has_location_field = id_fields
            .iter()
            .any(|field| resolve_case_insensitive_field(&filter_fields, field).is_some());
        if has_location_field {
            return false;
        }
    }
    let child_roles = schema_registry.field_roles_for_root(&root_field);

    let mut filter_candidates = Vec::new();
    if let Some(filter) = fetch_filter {
        collect_filter_string_constraints(&filter, &mut filter_candidates);
    }
    for step in &filter_rows_steps {
        if let PlanV2Op::FilterRows {
            source,
            field,
            operator: _,
            value,
        } = &step.op
        {
            if source != &fetch_id {
                continue;
            }
            if let serde_json::Value::String(s) = value
                && !s.trim().is_empty()
            {
                filter_candidates.push((field.clone(), s.clone()));
            }
        }
    }
    if filter_candidates.is_empty() {
        return false;
    }

    let mut candidate_values = Vec::new();
    for (field, value) in filter_candidates {
        let is_label_field = child_roles
            .label_fields
            .iter()
            .any(|label| label.eq_ignore_ascii_case(&field));
        let is_id_field = child_roles
            .id_fields
            .iter()
            .any(|id| id.eq_ignore_ascii_case(&field));
        let is_entity_key_field = child_roles
            .entity_key_fields
            .iter()
            .any(|id| id.eq_ignore_ascii_case(&field));
        if is_label_field {
            continue;
        }
        if (is_id_field || is_entity_key_field) && !is_label_like_value(&value) {
            continue;
        }
        if !value.chars().any(|c| c.is_ascii_alphabetic()) {
            continue;
        }
        candidate_values.push(value);
    }
    if candidate_values.is_empty() {
        return false;
    }

    let mut parent_candidates =
        sls_parent_relation_candidates(schema_registry, user_message, sls, &root_field, child_type);

    if parent_candidates.is_empty() {
        return false;
    }

    let (_, parent_root, relation_field) = parent_candidates.remove(0);

    let Some(parent_type) = schema_registry.query_return_type(&parent_root) else {
        return false;
    };
    let parent_roles = schema_registry.field_roles_for_root(&parent_root);
    let filter_fields = schema_registry.root_filter_fields(&parent_root);
    let mut clauses = Vec::new();
    for label in &parent_roles.label_fields {
        if !filter_fields.iter().any(|f| f.eq_ignore_ascii_case(label)) {
            continue;
        }
        if let Some(clause) = build_filter_clause_for_values(
            schema_registry,
            &parent_root,
            label,
            &candidate_values,
            Some("eq"),
        ) {
            clauses.push(clause);
        }
    }
    if clauses.is_empty() {
        return false;
    }
    let filter = if clauses.len() == 1 {
        clauses.remove(0)
    } else {
        serde_json::json!({ "or": clauses })
    };

    let mut parent_fields = parent_roles
        .label_fields
        .iter()
        .filter(|field| {
            schema_registry
                .object_field_names(parent_type)
                .is_some_and(|fields| fields.contains(*field))
        })
        .cloned()
        .collect::<Vec<_>>();
    if parent_fields.is_empty() {
        parent_fields = schema_registry.default_scalar_fields_for_root(&parent_root, 6);
    }
    let mut child_fields = schema_registry.default_scalar_fields_for_root(&root_field, 6);
    if child_fields.is_empty() {
        child_fields = child_roles.label_fields.clone();
    }

    let mut seen = HashSet::new();
    let mut fields = Vec::new();
    for field in parent_fields {
        let key = field.to_ascii_lowercase();
        if seen.insert(key) {
            fields.push(field);
        }
    }
    for field in child_fields {
        let nested = format!("{relation_field}.{field}");
        let key = nested.to_ascii_lowercase();
        if seen.insert(key) {
            fields.push(nested);
        }
    }

    if !plan.rewrites.iter().any(|r| r == "parent_relation_rewrite") {
        plan.rewrites.push("parent_relation_rewrite".to_string());
    }
    plan.notes.push(format!(
        "Rewrote child-root fetch into parent relation `{relation_field}` for label match."
    ));
    plan.steps = vec![PlanV2Step {
        id: fetch_id,
        op: PlanV2Op::Fetch {
            root_field: parent_root,
            fields,
            first,
            offset,
            filter: Some(filter),
            order: None,
        },
    }];
    true
}

pub(crate) fn scope_used_summary(
    planned_query_text: &str,
    executed_query_text: &str,
) -> serde_json::Value {
    fn is_placeholder_value(value: &str) -> bool {
        value.contains("${")
    }

    let planned = collect_query_scope(planned_query_text);
    let executed = collect_query_scope(executed_query_text);
    let planned_constraints = collect_scope_constraints(planned_query_text);
    let executed_constraints = collect_scope_constraints(executed_query_text);
    let mut matched = Vec::new();
    let mut missing = Vec::new();
    for constraint in &planned_constraints {
        let is_matched = executed_constraints.iter().any(|other| {
            constraint.root.eq_ignore_ascii_case(&other.root)
                && constraint.field.eq_ignore_ascii_case(&other.field)
                && scope_operator_preserves_constraint(&constraint.op, &other.op)
                && (constraint.values.iter().any(|v| is_placeholder_value(v))
                    || constraint
                        .values
                        .iter()
                        .any(|v| other.values.iter().any(|ov| scope_values_match(v, ov))))
        });
        if is_matched {
            matched.push(constraint.clone());
        } else {
            missing.push(constraint.clone());
        }
    }
    let missing_roots = planned
        .roots
        .iter()
        .filter(|r| !executed.roots.contains(*r))
        .cloned()
        .collect::<Vec<_>>();
    serde_json::json!({
        "matched_constraints": matched
            .into_iter()
            .map(|c| serde_json::json!({
                "root": c.root,
                "field": c.field,
                "op": c.op,
                "values": c.values
            }))
            .collect::<Vec<_>>(),
        "missing_constraints": missing
            .into_iter()
            .map(|c| serde_json::json!({
                "root": c.root,
                "field": c.field,
                "op": c.op,
                "values": c.values
            }))
            .collect::<Vec<_>>(),
        "planned_roots": planned.roots.into_iter().collect::<Vec<_>>(),
        "executed_roots": executed.roots.into_iter().collect::<Vec<_>>(),
        "missing_roots": missing_roots
    })
}

pub(crate) fn scope_guard_message(missing: &[String]) -> String {
    if let Some(message) = child_relation_backend_issue_message(missing) {
        return message;
    }
    format!(
        "I can’t answer with the requested scope because this execution did not include: {}.",
        missing.join(", ")
    )
}

fn child_relation_backend_issue_message(missing: &[String]) -> Option<String> {
    let child_row_gaps: Vec<&str> = missing
        .iter()
        .map(String::as_str)
        .filter(|item| item.ends_with(" returned no child rows"))
        .collect();
    if child_row_gaps.is_empty() {
        return None;
    }

    let relation_list = child_row_gaps
        .iter()
        .map(|item| item.trim_end_matches(" returned no child rows"))
        .collect::<Vec<_>>()
        .join(", ");

    Some(format!(
        "I found the requested parent entity, but I still can’t answer this scoped request because the backend returned no child rows for {relation_list}. This looks like a backend/schema relation issue rather than a true empty result."
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        PlanV2Op, build_filter_clause_for_values, extract_field_phrase_after_grouping_term,
        extract_identifier_candidates, scope_guard_message, synthesize_simple_fetch_plan,
    };
    use crate::schema_registry::SchemaRegistry;
    use crate::sls::IntentVocabulary;

    fn registry() -> SchemaRegistry {
        SchemaRegistry::new(include_str!("../schemas/consumer_schema.graphql"))
    }

    #[test]
    fn identifier_candidates_extract_quoted_labels_without_shape_inference() {
        let candidates =
            extract_identifier_candidates(r#"List turbines in wind farm "Wind Farm 1"."#);
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate == "Wind Farm 1"),
            "expected quoted candidate, got {candidates:?}"
        );

        let unquoted = extract_identifier_candidates("List turbines in wind farm Wind Farm 1.");
        assert!(
            !unquoted.iter().any(|candidate| candidate == "Wind Farm 1"),
            "unquoted numbered labels should not be inferred as identifier candidates: {unquoted:?}"
        );
    }

    #[test]
    fn identifier_candidates_deduplicate_case_insensitively() {
        let candidates =
            extract_identifier_candidates("Show details for \"WF4\" and wf4 and Wind Farm WF4.");
        let wf4_count = candidates
            .iter()
            .filter(|candidate| candidate.eq_ignore_ascii_case("WF4"))
            .count();
        assert_eq!(
            wf4_count, 1,
            "expected deduplicated candidates, got {candidates:?}"
        );
    }

    #[test]
    fn filter_builder_deduplicates_values_before_choosing_operator() {
        let reg = registry();
        let clause = build_filter_clause_for_values(
            &reg,
            "queryOffshoreWindFarm",
            "name",
            &[
                "Wind Farm 1".to_string(),
                "Wind Farm 1".to_string(),
                "wind farm 1".to_string(),
            ],
            Some("eq"),
        )
        .expect("expected filter clause");
        assert_eq!(
            clause,
            serde_json::json!({
                "name": {
                    "eq": "Wind Farm 1"
                }
            })
        );
    }

    #[test]
    fn simple_fetch_fallback_does_not_guess_count_by_aggregate() {
        let reg = registry();
        let sls = crate::sls::load_sls_merged(&reg, "sls.yaml").expect("expected SLS");
        assert!(
            synthesize_simple_fetch_plan(&reg, "Count turbines by stringName.", Some(&sls))
                .is_none(),
            "count/group planning should be left to the LLM"
        );
    }

    #[test]
    fn simple_fetch_fallback_does_not_guess_rank_for_top_n() {
        let reg = registry();
        let sls = crate::sls::load_sls_merged(&reg, "sls.yaml").expect("expected SLS");
        assert!(
            synthesize_simple_fetch_plan(
                &reg,
                "Top 5 turbines by accumulatedDowntime.",
                Some(&sls),
            )
            .is_none(),
            "ranking intent should be left to the LLM"
        );
    }

    #[test]
    fn simple_fetch_fallback_uses_sls_policy_denied_intents() {
        let reg = registry();
        let sls = crate::sls::load_sls_merged(&reg, "sls.yaml").expect("expected SLS");
        assert!(
            synthesize_simple_fetch_plan(
                &reg,
                "Compare tags where categoryDescription equals \"Weather\".",
                Some(&sls),
            )
            .is_none(),
            "SLS fallback policy should block compare intents even with explicit field constraints"
        );
        assert!(
            synthesize_simple_fetch_plan(
                &reg,
                "Show tags where categoryDescription equals \"Weather\".",
                Some(&sls),
            )
            .is_some(),
            "SLS fallback policy should still allow explicit simple lookup constraints"
        );
    }

    #[test]
    fn simple_fetch_fallback_does_not_guess_plain_name_entity_lookup() {
        let reg = registry();
        assert!(
            synthesize_simple_fetch_plan(&reg, "Show details for wind farm Wind Farm 1.", None)
                .is_none(),
            "plain-name entity lookup should be left to the planner, not deterministic fallback"
        );
    }

    #[test]
    fn simple_fetch_fallback_allows_unique_explicit_field_constraint() {
        let reg = registry();
        let sls = crate::sls::load_sls_merged(&reg, "sls.yaml").expect("expected SLS");
        let plan = synthesize_simple_fetch_plan(
            &reg,
            "Show tags where categoryDescription equals \"Weather\".",
            Some(&sls),
        )
        .expect("expected fallback plan");
        let Some(fetch) = plan.steps.first() else {
            panic!("expected fetch step");
        };
        match &fetch.op {
            PlanV2Op::Fetch {
                root_field, filter, ..
            } => {
                assert_eq!(root_field, "queryTag");
                assert_eq!(
                    filter.as_ref(),
                    Some(&serde_json::json!({
                        "categoryDescription": { "eq": "Weather" }
                    }))
                );
            }
            other => panic!("expected fetch op, got {other:?}"),
        }
    }

    #[test]
    fn simple_fetch_fallback_does_not_parse_operator_words_without_sls_vocabulary() {
        let reg = registry();
        assert!(
            synthesize_simple_fetch_plan(
                &reg,
                "Show tags where categoryDescription equals \"Weather\".",
                None,
            )
            .is_none(),
            "explicit operator words should come from SLS vocabulary, not Rust defaults"
        );
    }

    #[test]
    fn simple_fetch_fallback_uses_sls_operator_vocabulary() {
        let reg = registry();
        let mut sls = crate::sls::load_sls_merged(&reg, "sls.yaml").expect("expected SLS");
        sls.intent_vocabulary.filter_eq = vec!["matches".to_string()];
        sls.intent_vocabulary.filter_contains = Vec::new();

        let plan = synthesize_simple_fetch_plan(
            &reg,
            "Show tags where categoryDescription matches \"Weather\".",
            Some(&sls),
        )
        .expect("expected custom SLS filter operator to drive fallback");
        let Some(fetch) = plan.steps.first() else {
            panic!("expected fetch step");
        };
        match &fetch.op {
            PlanV2Op::Fetch { filter, .. } => {
                assert_eq!(
                    filter.as_ref(),
                    Some(&serde_json::json!({
                        "categoryDescription": { "eq": "Weather" }
                    }))
                );
            }
            other => panic!("expected fetch op, got {other:?}"),
        }

        assert!(
            synthesize_simple_fetch_plan(
                &reg,
                "Show tags where categoryDescription equals \"Weather\".",
                Some(&sls),
            )
            .is_none(),
            "removed operator terms should not keep working as Rust literals"
        );
    }

    #[test]
    fn grouping_field_phrase_uses_sls_vocabulary() {
        let mut vocabulary = IntentVocabulary {
            group_by: vec!["grouped by".to_string()],
            ..IntentVocabulary::default()
        };
        assert_eq!(
            extract_field_phrase_after_grouping_term(
                "Show turbines grouped by stringName.",
                &vocabulary
            )
            .as_deref(),
            Some("stringName")
        );
        vocabulary.group_by = vec!["using".to_string()];
        assert!(
            extract_field_phrase_after_grouping_term("Show turbines by stringName.", &vocabulary)
                .is_none(),
            "removed grouping term should not keep working as a Rust literal"
        );
    }

    #[test]
    fn simple_fetch_fallback_uses_identifier_candidates_for_substation_lookup() {
        let reg = registry();
        let plan =
            synthesize_simple_fetch_plan(&reg, "Show offshore substation OSS-003 details.", None)
                .expect("expected fallback plan");
        let Some(fetch) = plan.steps.first() else {
            panic!("expected fetch step");
        };
        match &fetch.op {
            PlanV2Op::Fetch {
                root_field, filter, ..
            } => {
                assert!(
                    root_field == "queryOffshoreSubstation"
                        || root_field == "getOffshoreSubstation",
                    "expected offshore substation lookup root, got {root_field}"
                );
                let filter = filter.as_ref().expect("expected fallback filter");
                let clauses = filter.get("or").and_then(|value| value.as_array());
                assert!(
                    clauses
                        .map(|clauses| clauses.iter().any(|clause| {
                            clause
                                == &serde_json::json!({
                                    "id": { "eq": "OSS-003" }
                                })
                        }))
                        .unwrap_or_else(
                            || filter.get("id") == Some(&serde_json::json!({ "eq": "OSS-003" }))
                        ),
                );
            }
            other => panic!("expected fetch op, got {other:?}"),
        }
    }

    #[test]
    fn scope_guard_message_explains_empty_child_relation_as_backend_issue() {
        let message = scope_guard_message(&[
            "queryOffshoreWindFarm.hasOffshoreWindTurbine returned no child rows".to_string(),
        ]);

        assert!(message.contains("I found the requested parent entity"));
        assert!(message.contains("queryOffshoreWindFarm.hasOffshoreWindTurbine"));
        assert!(message.contains("backend/schema relation issue"));
    }

    #[test]
    fn scope_guard_message_keeps_generic_text_for_other_missing_scope_items() {
        let message = scope_guard_message(&["queryOffshoreWindTurbine.filter.name".to_string()]);

        assert_eq!(
            message,
            "I can’t answer with the requested scope because this execution did not include: queryOffshoreWindTurbine.filter.name."
        );
    }
}
