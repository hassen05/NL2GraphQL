#![allow(
    clippy::redundant_closure_for_method_calls,
    clippy::redundant_pub_crate,
    clippy::similar_names,
    clippy::unreadable_literal
)]

use crate::metric_formula::{
    AggFunc, AggOp, MetricExpr, collect_agg_funcs, eval_metric_expr, parse_metric_formula,
};
use crate::planner::MetricSpec;
use std::collections::HashMap;

#[derive(Clone, Debug, Default)]
pub(crate) struct LocationFieldHints {
    pub(crate) latitude_keys: std::collections::HashSet<String>,
    pub(crate) longitude_keys: std::collections::HashSet<String>,
    pub(crate) geo_object_keys: std::collections::HashSet<String>,
    geo_latitude_member_keys: std::collections::HashSet<String>,
    geo_longitude_member_keys: std::collections::HashSet<String>,
}

impl LocationFieldHints {
    pub(crate) fn from_schema_lists(
        latitude_fields: &[String],
        longitude_fields: &[String],
        geo_object_fields: &[String],
    ) -> Self {
        fn add_field_key(keys: &mut std::collections::HashSet<String>, field: &str) {
            let lower = field.to_lowercase();
            keys.insert(lower.clone());
            if let Some((_, tail)) = lower.rsplit_once('.') {
                keys.insert(tail.to_string());
            }
        }

        fn lower_set(items: &[String]) -> std::collections::HashSet<String> {
            let mut keys = std::collections::HashSet::new();
            for item in items {
                add_field_key(&mut keys, item);
            }
            keys
        }
        let latitude_keys = lower_set(latitude_fields);
        let longitude_keys = lower_set(longitude_fields);
        let geo_object_keys = lower_set(geo_object_fields);

        let object_coordinate_fields = latitude_keys
            .intersection(&longitude_keys)
            .chain(geo_object_keys.iter())
            .cloned()
            .collect::<std::collections::HashSet<_>>();
        let mut geo_latitude_member_keys = std::collections::HashSet::new();
        let mut geo_longitude_member_keys = std::collections::HashSet::new();
        if !object_coordinate_fields.is_empty() {
            geo_latitude_member_keys.extend(["lat".to_string(), "latitude".to_string()]);
            geo_longitude_member_keys.extend([
                "lng".to_string(),
                "lon".to_string(),
                "longitude".to_string(),
            ]);
        }

        Self {
            latitude_keys,
            longitude_keys,
            geo_object_keys,
            geo_latitude_member_keys,
            geo_longitude_member_keys,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct RecordFieldHints {
    pub(crate) time_keys: std::collections::HashSet<String>,
    pub(crate) identifier_keys: std::collections::HashSet<String>,
    pub(crate) label_keys: std::collections::HashSet<String>,
}

impl RecordFieldHints {
    pub(crate) fn from_schema_lists(
        time_fields: &[String],
        identifier_fields: &[String],
        label_fields: &[String],
    ) -> Self {
        fn lower_set(items: &[String]) -> std::collections::HashSet<String> {
            items.iter().map(|s| s.to_lowercase()).collect()
        }
        Self {
            time_keys: lower_set(time_fields),
            identifier_keys: lower_set(identifier_fields),
            label_keys: lower_set(label_fields),
        }
    }
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

fn values_at_path<'a>(row: &'a serde_json::Value, path: &str) -> Vec<&'a serde_json::Value> {
    fn visit<'a>(
        current: &'a serde_json::Value,
        parts: &[&str],
        out: &mut Vec<&'a serde_json::Value>,
    ) {
        if parts.is_empty() {
            out.push(current);
            return;
        }
        match current {
            serde_json::Value::Array(items) => {
                for item in items {
                    visit(item, parts, out);
                }
            }
            serde_json::Value::Object(obj) => {
                if let Some(next) = obj.get(parts[0]) {
                    visit(next, &parts[1..], out);
                }
            }
            _ => {}
        }
    }

    let parts = path.split('.').collect::<Vec<_>>();
    let mut out = Vec::new();
    visit(row, &parts, &mut out);
    out
}

fn contains_field_hint(
    hints: Option<&std::collections::HashSet<String>>,
    field_name: &str,
) -> bool {
    let Some(hints) = hints else {
        return false;
    };
    hints.contains(&field_name.to_lowercase())
}

#[derive(Clone, Debug, Default)]
struct MetricStats {
    count: i64,
    sum: f64,
    sum_sq: f64,
    min: Option<f64>,
    max: Option<f64>,
}

impl MetricStats {
    fn observe(&mut self, value: f64) {
        self.count += 1;
        self.sum += value;
        self.sum_sq += value * value;
        self.min = Some(self.min.map_or(value, |m| m.min(value)));
        self.max = Some(self.max.map_or(value, |m| m.max(value)));
    }
}

pub(crate) fn aggregate_metrics(
    rows: &[serde_json::Value],
    group_by: &[String],
    metrics: &[MetricSpec],
) -> Vec<serde_json::Value> {
    if metrics.is_empty() {
        return Vec::new();
    }

    struct MetricPlan {
        metric: MetricSpec,
        expr: Option<MetricExpr>,
        aggs: Vec<AggFunc>,
    }

    fn agg_for_metric(metric: &MetricSpec) -> Option<AggFunc> {
        match metric {
            MetricSpec::Count => Some(AggFunc {
                op: AggOp::Count,
                field: None,
            }),
            MetricSpec::Sum { field } => Some(AggFunc {
                op: AggOp::Sum,
                field: Some(field.clone()),
            }),
            MetricSpec::Avg { field } => Some(AggFunc {
                op: AggOp::Avg,
                field: Some(field.clone()),
            }),
            MetricSpec::Min { field } => Some(AggFunc {
                op: AggOp::Min,
                field: Some(field.clone()),
            }),
            MetricSpec::Max { field } => Some(AggFunc {
                op: AggOp::Max,
                field: Some(field.clone()),
            }),
            MetricSpec::Stddev { field } => Some(AggFunc {
                op: AggOp::Stddev,
                field: Some(field.clone()),
            }),
            MetricSpec::Ref { .. } | MetricSpec::Formula { .. } => None,
        }
    }

    let mut plans = Vec::new();
    let mut required_aggs = std::collections::HashSet::new();
    for metric in metrics {
        match metric {
            MetricSpec::Formula { expr, .. } => {
                let parsed = parse_metric_formula(expr).ok();
                let aggs = parsed.as_ref().map(collect_agg_funcs).unwrap_or_default();
                for agg in &aggs {
                    required_aggs.insert(agg.clone());
                }
                plans.push(MetricPlan {
                    metric: metric.clone(),
                    expr: parsed,
                    aggs,
                });
            }
            _ => {
                if let Some(agg) = agg_for_metric(metric) {
                    required_aggs.insert(agg.clone());
                    plans.push(MetricPlan {
                        metric: metric.clone(),
                        expr: None,
                        aggs: vec![agg],
                    });
                } else {
                    plans.push(MetricPlan {
                        metric: metric.clone(),
                        expr: None,
                        aggs: Vec::new(),
                    });
                }
            }
        }
    }

    let required_aggs = required_aggs.into_iter().collect::<Vec<_>>();

    fn init_stats(aggs: &[AggFunc]) -> std::collections::HashMap<AggFunc, MetricStats> {
        let mut out = std::collections::HashMap::new();
        for agg in aggs {
            out.insert(agg.clone(), MetricStats::default());
        }
        out
    }

    fn update_stats_for_row(stats: &mut MetricStats, agg: &AggFunc, row: &serde_json::Value) {
        match agg.op {
            AggOp::Count => {
                if let Some(field) = agg.field.as_deref() {
                    stats.count += values_at_path(row, field)
                        .into_iter()
                        .filter(|value| !value.is_null())
                        .count() as i64;
                } else {
                    stats.count += 1;
                }
            }
            AggOp::Sum | AggOp::Avg | AggOp::Min | AggOp::Max | AggOp::Stddev => {
                let Some(field) = agg.field.as_deref() else {
                    return;
                };
                for value in values_at_path(row, field) {
                    if let Some(v) = value.as_f64() {
                        stats.observe(v);
                    }
                }
            }
        }
    }

    fn agg_value(agg: &AggFunc, stats: &MetricStats) -> Option<f64> {
        match agg.op {
            AggOp::Count => Some(stats.count as f64),
            AggOp::Sum => (stats.count > 0).then_some(stats.sum),
            AggOp::Avg => (stats.count > 0).then_some(stats.sum / stats.count as f64),
            AggOp::Min => stats.min,
            AggOp::Max => stats.max,
            AggOp::Stddev => {
                if stats.count > 1 {
                    let mean = stats.sum / stats.count as f64;
                    let variance = (stats.sum_sq / stats.count as f64) - mean * mean;
                    Some(variance.max(0.0).sqrt())
                } else if stats.count == 1 {
                    Some(0.0)
                } else {
                    None
                }
            }
        }
    }

    type GroupBucket = (
        Vec<(String, serde_json::Value)>,
        std::collections::HashMap<AggFunc, MetricStats>,
    );
    let mut groups: std::collections::BTreeMap<String, GroupBucket> =
        std::collections::BTreeMap::new();

    for row in rows {
        let key_fields = group_by
            .iter()
            .map(|f| {
                (
                    f.clone(),
                    value_at_path(row, f).unwrap_or(serde_json::Value::Null),
                )
            })
            .collect::<Vec<_>>();
        let key = serde_json::to_string(&key_fields).unwrap_or_else(|_| "[]".to_string());
        let entry = groups
            .entry(key)
            .or_insert_with(|| (key_fields, init_stats(&required_aggs)));
        for agg in &required_aggs {
            if let Some(stats) = entry.1.get_mut(agg) {
                update_stats_for_row(stats, agg, row);
            }
        }
    }

    groups
        .into_values()
        .map(|(fields, stats)| {
            let mut obj = serde_json::Map::new();
            for (k, v) in fields {
                obj.insert(k, v);
            }
            for plan in &plans {
                let key = plan.metric.output_key();
                let value = if let Some(expr) = &plan.expr {
                    let lookup = |agg: &AggFunc| stats.get(agg).and_then(|s| agg_value(agg, s));
                    eval_metric_expr(expr, lookup)
                        .map(|v| serde_json::json!(v))
                        .unwrap_or(serde_json::Value::Null)
                } else if let Some(agg) = plan.aggs.first() {
                    if matches!(plan.metric, MetricSpec::Count) {
                        stats
                            .get(agg)
                            .map(|s| serde_json::json!(s.count))
                            .unwrap_or(serde_json::Value::Null)
                    } else {
                        stats
                            .get(agg)
                            .and_then(|s| agg_value(agg, s))
                            .map(|v| serde_json::json!(v))
                            .unwrap_or(serde_json::Value::Null)
                    }
                } else {
                    serde_json::Value::Null
                };
                obj.insert(key, value);
            }
            serde_json::Value::Object(obj)
        })
        .collect()
}

pub(crate) fn rank_rows(
    rows: &[serde_json::Value],
    by: &str,
    direction: &str,
    limit: Option<usize>,
) -> Vec<serde_json::Value> {
    let mut out = rows.to_vec();
    out.sort_by(|a, b| {
        let av = a.get(by).cloned().unwrap_or(serde_json::Value::Null);
        let bv = b.get(by).cloned().unwrap_or(serde_json::Value::Null);
        let ord = match (av.as_f64(), bv.as_f64()) {
            (Some(na), Some(nb)) => na.partial_cmp(&nb).unwrap_or(std::cmp::Ordering::Equal),
            _ => {
                let sa = av
                    .as_str()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| av.to_string());
                let sb = bv
                    .as_str()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| bv.to_string());
                sa.cmp(&sb)
            }
        };
        if direction.eq_ignore_ascii_case("asc") {
            ord
        } else {
            ord.reverse()
        }
    });
    if let Some(n) = limit {
        out.truncate(n);
    }
    out
}

fn json_as_f64(value: &serde_json::Value) -> Option<f64> {
    match value {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.parse::<f64>().ok(),
        _ => None,
    }
}

fn row_matches_filter(
    row: &serde_json::Value,
    field: &str,
    operator: &str,
    value: &serde_json::Value,
) -> bool {
    let Some(actual) = value_at_path(row, field) else {
        return false;
    };
    match operator.trim().to_ascii_lowercase().as_str() {
        "eq" => actual == *value,
        "ne" => actual != *value,
        "contains" => {
            let actual_text = actual
                .as_str()
                .map(std::string::ToString::to_string)
                .unwrap_or_else(|| actual.to_string())
                .to_ascii_lowercase();
            let expected_text = value
                .as_str()
                .map(std::string::ToString::to_string)
                .unwrap_or_else(|| value.to_string())
                .to_ascii_lowercase();
            actual_text.contains(&expected_text)
        }
        "gt" => json_as_f64(&actual)
            .zip(json_as_f64(value))
            .is_some_and(|(a, b)| a > b),
        "gte" => json_as_f64(&actual)
            .zip(json_as_f64(value))
            .is_some_and(|(a, b)| a >= b),
        "lt" => json_as_f64(&actual)
            .zip(json_as_f64(value))
            .is_some_and(|(a, b)| a < b),
        "lte" => json_as_f64(&actual)
            .zip(json_as_f64(value))
            .is_some_and(|(a, b)| a <= b),
        _ => false,
    }
}

pub(crate) fn filter_rows(
    rows: &[serde_json::Value],
    field: &str,
    operator: &str,
    value: &serde_json::Value,
) -> Vec<serde_json::Value> {
    rows.iter()
        .filter(|row| row_matches_filter(row, field, operator, value))
        .cloned()
        .collect()
}

pub(crate) fn render_ranked_count_summary(rows: &[serde_json::Value]) -> String {
    if rows.is_empty() {
        return "No matching records found.".to_string();
    }
    let mut ranked = rows.to_vec();
    ranked.sort_by(|a, b| {
        let ac = a.get("count").and_then(|v| v.as_i64()).unwrap_or(0);
        let bc = b.get("count").and_then(|v| v.as_i64()).unwrap_or(0);
        bc.cmp(&ac)
    });
    let top = ranked.iter().take(5).collect::<Vec<_>>();
    fn format_group_label(row: &serde_json::Value) -> String {
        let Some(obj) = row.as_object() else {
            return row.to_string();
        };
        let mut keys = obj
            .keys()
            .filter(|k| k.as_str() != "count")
            .filter(|k| {
                if let Some(base) = k.strip_suffix("_label") {
                    return !obj.contains_key(base);
                }
                true
            })
            .cloned()
            .collect::<Vec<_>>();
        keys.sort();
        let parts = keys
            .into_iter()
            .filter_map(|k| {
                let label_key = format!("{k}_label");
                if let Some(label) = obj.get(&label_key).and_then(|value| value.as_str()) {
                    return Some(format!("{k}={label}"));
                }
                let value = obj.get(&k)?;
                match value {
                    serde_json::Value::String(s) => Some(format!("{k}={s}")),
                    serde_json::Value::Number(_) | serde_json::Value::Bool(_) => {
                        Some(format!("{k}={value}"))
                    }
                    _ => None,
                }
            })
            .collect::<Vec<_>>();
        if parts.is_empty() {
            "group".to_string()
        } else {
            parts.join(", ")
        }
    }
    let parts = top
        .iter()
        .map(|r| {
            let count = r.get("count").and_then(|v| v.as_i64()).unwrap_or(0);
            let label = format_group_label(r);
            format!("{label}: {count}")
        })
        .collect::<Vec<_>>();
    let max_count = ranked
        .first()
        .and_then(|r| r.get("count"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let leaders = ranked
        .iter()
        .filter(|r| r.get("count").and_then(|v| v.as_i64()).unwrap_or(0) == max_count)
        .collect::<Vec<_>>();
    let leader_names = leaders
        .iter()
        .map(|r| format_group_label(r))
        .collect::<Vec<_>>();
    let group_fields = ranked
        .first()
        .and_then(|r| r.as_object())
        .map(|obj| {
            let mut keys = obj
                .keys()
                .filter(|k| k.as_str() != "count")
                .cloned()
                .collect::<Vec<_>>();
            keys.sort();
            keys
        })
        .unwrap_or_default();
    let grouping_text = if group_fields.is_empty() {
        String::new()
    } else {
        format!(" by {}", group_fields.join(", "))
    };
    format!(
        "Highest counts{}: {} ({} each). Top ranking: {}.",
        grouping_text,
        leader_names.join(", "),
        max_count,
        parts.join(", ")
    )
}

fn is_metric_key(key: &str) -> bool {
    key == "count"
        || key.starts_with("sum_")
        || key.starts_with("avg_")
        || key.starts_with("min_")
        || key.starts_with("max_")
        || key.starts_with("stddev_")
        || key.starts_with("metric_")
}

fn row_metric_keys(row: &serde_json::Value) -> Vec<String> {
    let Some(obj) = row.as_object() else {
        return Vec::new();
    };
    let mut keys = obj
        .keys()
        .filter(|k| is_metric_key(k))
        .cloned()
        .collect::<Vec<_>>();
    keys.sort();
    keys
}

pub(crate) fn row_has_metric_keys(row: &serde_json::Value) -> bool {
    !row_metric_keys(row).is_empty()
}

fn row_group_keys(row: &serde_json::Value) -> Vec<String> {
    let Some(obj) = row.as_object() else {
        return Vec::new();
    };
    let mut keys = obj
        .keys()
        .filter(|k| {
            if is_metric_key(k) {
                return false;
            }
            if let Some(base) = k.strip_suffix("_label") {
                return !obj.contains_key(base);
            }
            true
        })
        .cloned()
        .collect::<Vec<_>>();
    keys.sort();
    keys
}

fn format_key_values(row: &serde_json::Value, keys: &[String]) -> Vec<String> {
    let Some(obj) = row.as_object() else {
        return Vec::new();
    };
    keys.iter()
        .filter_map(|k| {
            let label_key = format!("{k}_label");
            if let Some(label) = obj.get(&label_key).and_then(|v| v.as_str()) {
                return Some(format!("{k}={label}"));
            }
            obj.get(k).map(|v| match v {
                serde_json::Value::String(s) => format!("{k}={s}"),
                serde_json::Value::Number(_) | serde_json::Value::Bool(_) => format!("{k}={v}"),
                serde_json::Value::Null => format!("{k}=null"),
                _ => format!("{k}={v}"),
            })
        })
        .collect()
}

pub(crate) fn render_aggregate_summary(rows: &[serde_json::Value]) -> String {
    if rows.is_empty() {
        return "No matching records found.".to_string();
    }
    let metric_keys = row_metric_keys(&rows[0]);
    let group_keys = row_group_keys(&rows[0]);
    let preview = rows
        .iter()
        .take(5)
        .map(|row| {
            let group_part = format_key_values(row, &group_keys).join(", ");
            let metric_part = format_key_values(row, &metric_keys).join(", ");
            match (group_part.is_empty(), metric_part.is_empty()) {
                (true, true) => row.to_string(),
                (true, false) => metric_part,
                (false, true) => group_part,
                (false, false) => format!("{group_part} -> {metric_part}"),
            }
        })
        .collect::<Vec<_>>()
        .join(" | ");
    if group_keys.is_empty() {
        format!("Aggregated results: {preview}.")
    } else {
        format!(
            "Aggregated results by {}: {}.",
            group_keys.join(", "),
            preview
        )
    }
}

pub(crate) fn render_aggregate_result_summary(rows: &[serde_json::Value]) -> String {
    if rows.is_empty() {
        return "No matching records found.".to_string();
    }
    let metric_keys = row_metric_keys(&rows[0]);
    let has_only_count = metric_keys.len() == 1 && metric_keys[0] == "count";
    if has_only_count {
        render_ranked_count_summary(rows)
    } else {
        render_aggregate_summary(rows)
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct RowDisplayRoles {
    pub(crate) label_fields: Vec<String>,
    pub(crate) entity_key_fields: Vec<String>,
    pub(crate) id_fields: Vec<String>,
    pub(crate) numeric_fields: Vec<String>,
    pub(crate) time_fields: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct RowsDisplayHints {
    pub(crate) parent_roles: RowDisplayRoles,
    pub(crate) relation_roles: HashMap<String, RowDisplayRoles>,
    pub(crate) relation_record_types: HashMap<String, String>,
}

impl RowDisplayRoles {
    pub(crate) fn from_field_roles(roles: &crate::domain_config::FieldRoleSet) -> Self {
        Self {
            label_fields: roles.label_fields.clone(),
            entity_key_fields: roles.entity_key_fields.clone(),
            id_fields: roles.id_fields.clone(),
            numeric_fields: roles.numeric_fields.clone(),
            time_fields: roles.time_fields.clone(),
        }
    }

    fn field_rank(&self, field: &str) -> (u8, usize) {
        fn pos(fields: &[String], field: &str) -> Option<usize> {
            fields.iter().position(|candidate| candidate == field)
        }

        if let Some(idx) = pos(&self.label_fields, field) {
            return (0, idx);
        }
        if let Some(idx) = pos(&self.entity_key_fields, field) {
            return (1, idx);
        }
        if let Some(idx) = pos(&self.id_fields, field) {
            return (2, idx);
        }
        if let Some(idx) = pos(&self.time_fields, field) {
            return (3, idx);
        }
        if let Some(idx) = pos(&self.numeric_fields, field) {
            return (4, idx);
        }
        (5, usize::MAX)
    }

    fn is_numeric(&self, field: &str) -> bool {
        self.numeric_fields
            .iter()
            .any(|candidate| candidate == field)
    }
}

fn scalar_display(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(_) | serde_json::Value::Bool(_) => Some(value.to_string()),
        _ => None,
    }
}

fn scalar_display_for_field(
    key: &str,
    value: &serde_json::Value,
    roles: Option<&RowDisplayRoles>,
) -> Option<String> {
    if roles.is_some_and(|role_set| role_set.is_numeric(key))
        && let Some(number) = value.as_f64()
    {
        if number.fract().abs() < f64::EPSILON {
            return Some(format!("{number:.0}"));
        }
        return Some(format!("{number:.2}"));
    }
    scalar_display(value)
}

fn display_field_name(key: &str) -> String {
    let mut out = String::new();
    let mut prev_was_lower_or_digit = false;
    for ch in key.chars() {
        if ch == '_' || ch == '-' {
            if !out.ends_with(' ') {
                out.push(' ');
            }
            prev_was_lower_or_digit = false;
            continue;
        }
        if ch.is_ascii_uppercase() && prev_was_lower_or_digit {
            out.push(' ');
        }
        out.push(ch.to_ascii_lowercase());
        prev_was_lower_or_digit = ch.is_ascii_lowercase() || ch.is_ascii_digit();
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn sorted_scalar_parts(
    obj: &serde_json::Map<String, serde_json::Value>,
    roles: Option<&RowDisplayRoles>,
    skip_fields: &[String],
    limit: usize,
) -> Vec<String> {
    let mut values = obj
        .iter()
        .filter(|(key, _)| !skip_fields.iter().any(|skip| skip == *key))
        .filter_map(|(key, value)| {
            scalar_display_for_field(key, value, roles).map(|display| (key.clone(), display))
        })
        .collect::<Vec<_>>();
    values.sort_by(|(a, _), (b, _)| {
        let a_rank = roles.map_or((5, usize::MAX), |role_set| role_set.field_rank(a));
        let b_rank = roles.map_or((5, usize::MAX), |role_set| role_set.field_rank(b));
        a_rank.cmp(&b_rank).then_with(|| a.cmp(b))
    });
    values
        .into_iter()
        .take(limit)
        .map(|(key, display)| format!("{}: {display}", display_field_name(&key)))
        .collect()
}

fn first_role_scalar(
    obj: &serde_json::Map<String, serde_json::Value>,
    roles: &RowDisplayRoles,
    fields: &[String],
) -> Option<(String, String)> {
    fields.iter().find_map(|field| {
        obj.get(field)
            .and_then(|value| scalar_display_for_field(field, value, Some(roles)))
            .map(|display| (field.clone(), display))
    })
}

fn record_identity(
    obj: &serde_json::Map<String, serde_json::Value>,
    roles: Option<&RowDisplayRoles>,
) -> Option<(String, Vec<String>)> {
    let roles = roles?;
    let (label_field, label) = first_role_scalar(obj, roles, &roles.label_fields)?;
    let mut used = vec![label_field];
    let key_fields = roles
        .entity_key_fields
        .iter()
        .chain(roles.id_fields.iter())
        .filter(|field| !used.iter().any(|used_field| used_field == *field))
        .cloned()
        .collect::<Vec<_>>();
    let key = first_role_scalar(obj, roles, &key_fields).and_then(|(field, value)| {
        used.push(field);
        if value == label { None } else { Some(value) }
    });
    let text = key
        .map(|value| format!("{label} ({value})"))
        .unwrap_or(label);
    Some((text, used))
}

fn record_summary(
    obj: &serde_json::Map<String, serde_json::Value>,
    roles: Option<&RowDisplayRoles>,
    detail_limit: usize,
) -> String {
    if let Some((identity, used_fields)) = record_identity(obj, roles) {
        let details = sorted_scalar_parts(obj, roles, &used_fields, detail_limit);
        if details.is_empty() {
            identity
        } else {
            format!("{identity} - {}", details.join(", "))
        }
    } else {
        let parts = sorted_scalar_parts(obj, roles, &[], detail_limit);
        if parts.is_empty() {
            serde_json::Value::Object(obj.clone()).to_string()
        } else {
            parts.join(", ")
        }
    }
}

fn parent_context_summary(
    obj: &serde_json::Map<String, serde_json::Value>,
    roles: &RowDisplayRoles,
) -> String {
    if let Some((identity, _)) = record_identity(obj, Some(roles)) {
        return identity;
    }
    let parts = sorted_scalar_parts(obj, Some(roles), &[], 2);
    if parts.is_empty() {
        "selected parent record".to_string()
    } else {
        parts.join(", ")
    }
}

fn display_type_name(type_name: &str) -> String {
    display_field_name(type_name)
}

pub(crate) fn render_rows_summary_with_hints(
    rows: &[serde_json::Value],
    sample_limit: usize,
    hints: &RowsDisplayHints,
) -> String {
    const NESTED_LIST_PREVIEW_ROWS: usize = 12;

    if rows.len() != 1 {
        return render_rows_summary(rows, sample_limit);
    }
    let Some(parent) = rows[0].as_object() else {
        return render_rows_summary(rows, sample_limit);
    };
    let Some((relation_key, items)) = parent
        .iter()
        .filter_map(|(key, value)| value.as_array().map(|items| (key, items)))
        .find(|(_, items)| !items.is_empty())
    else {
        return render_rows_summary(rows, sample_limit);
    };

    let relation_roles = hints.relation_roles.get(relation_key);
    let record_type = hints.relation_record_types.get(relation_key).map_or_else(
        || display_field_name(relation_key),
        |ty| display_type_name(ty),
    );
    let parent_text = parent_context_summary(parent, &hints.parent_roles);
    let take_n = sample_limit.max(NESTED_LIST_PREVIEW_ROWS).min(items.len());
    let preview = items
        .iter()
        .take(take_n)
        .map(|item| match item {
            serde_json::Value::Object(obj) => record_summary(obj, relation_roles, 3),
            _ => scalar_display(item).unwrap_or_else(|| item.to_string()),
        })
        .enumerate()
        .map(|(idx, text)| format!("{}. {text}", idx + 1))
        .collect::<Vec<_>>()
        .join("; ");
    let suffix = if take_n < items.len() {
        format!(
            " Showing first {take_n}; {} more not shown.",
            items.len() - take_n
        )
    } else {
        String::new()
    };
    format!(
        "Found {} {} record(s) for {}: {}.{}",
        items.len(),
        record_type,
        parent_text,
        preview,
        suffix
    )
}

pub(crate) fn render_rows_summary(rows: &[serde_json::Value], sample_limit: usize) -> String {
    const NESTED_LIST_PREVIEW_ROWS: usize = 12;

    fn field_priority(key: &str) -> i32 {
        let lower = key.to_ascii_lowercase();
        if lower == "name" {
            return 0;
        }
        if lower.ends_with("name") {
            return 1;
        }
        if lower.contains("label") || lower.contains("title") || lower.ends_with("description") {
            return 2;
        }
        if lower.contains("time") || lower.contains("date") {
            return 10;
        }
        if lower.ends_with("id") || lower.ends_with("uid") {
            return 20;
        }
        50
    }

    fn sorted_scalar_parts(
        obj: &serde_json::Map<String, serde_json::Value>,
        limit: usize,
    ) -> Vec<String> {
        let mut keys = obj
            .iter()
            .filter_map(|(key, value)| scalar_display(value).map(|display| (key.clone(), display)))
            .collect::<Vec<_>>();
        keys.sort_by(|(a, _), (b, _)| a.cmp(b));
        keys.into_iter()
            .take(limit)
            .map(|(key, display)| format!("{key}: {display}"))
            .collect()
    }

    fn format_child_object(obj: &serde_json::Map<String, serde_json::Value>) -> String {
        let parts = sorted_scalar_parts(obj, 4);
        if parts.is_empty() {
            serde_json::Value::Object(obj.clone()).to_string()
        } else {
            parts.join(", ")
        }
    }

    fn format_nested_list(
        parent: &serde_json::Map<String, serde_json::Value>,
        relation_key: &str,
        items: &[serde_json::Value],
        sample_limit: usize,
    ) -> String {
        let parent_context = sorted_scalar_parts(parent, 3);
        let parent_text = if parent_context.is_empty() {
            "the parent row".to_string()
        } else {
            format!("parent row ({})", parent_context.join(", "))
        };
        let take_n = sample_limit.max(NESTED_LIST_PREVIEW_ROWS).min(items.len());
        let preview = items
            .iter()
            .take(take_n)
            .map(|item| match item {
                serde_json::Value::Object(obj) => format_child_object(obj),
                _ => scalar_display(item).unwrap_or_else(|| item.to_string()),
            })
            .enumerate()
            .map(|(idx, text)| {
                let item_number = idx + 1;
                format!("{item_number}. {text}")
            })
            .collect::<Vec<_>>()
            .join("; ");
        let suffix = if take_n < items.len() {
            format!(
                " Showing first {take_n}; {} more not shown.",
                items.len() - take_n
            )
        } else {
            String::new()
        };
        format!(
            "Found {} child item(s) in `{}` for {}: {}.{}",
            items.len(),
            relation_key,
            parent_text,
            preview,
            suffix
        )
    }

    fn nested_list_summary(rows: &[serde_json::Value], sample_limit: usize) -> Option<String> {
        if rows.len() != 1 {
            return None;
        }
        let parent = rows[0].as_object()?;
        let (relation_key, items) = parent
            .iter()
            .filter_map(|(key, value)| value.as_array().map(|items| (key, items)))
            .find(|(_, items)| !items.is_empty())?;
        Some(format_nested_list(
            parent,
            relation_key,
            items,
            sample_limit,
        ))
    }

    if rows.is_empty() {
        return "No matching records found.".to_string();
    }
    if let Some(summary) = nested_list_summary(rows, sample_limit) {
        return summary;
    }
    let take_n = sample_limit.max(1).min(rows.len());
    let sample = rows
        .iter()
        .take(take_n)
        .map(|row| {
            if let Some(obj) = row.as_object() {
                let mut keys = obj.keys().cloned().collect::<Vec<_>>();
                keys.sort_by(|a, b| {
                    field_priority(a)
                        .cmp(&field_priority(b))
                        .then_with(|| a.cmp(b))
                });
                let parts = keys
                    .into_iter()
                    .take(6)
                    .filter_map(|k| {
                        let v = &obj[&k];
                        if let Some(s) = v.as_str() {
                            Some(format!("{k}: {s}"))
                        } else if v.is_array() || v.is_object() {
                            None
                        } else {
                            Some(format!("{k}: {v}"))
                        }
                    })
                    .collect::<Vec<_>>();
                parts.join(", ")
            } else {
                row.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" | ");
    if take_n < rows.len() {
        format!(
            "Found {} result(s), showing first {}: {}.",
            rows.len(),
            take_n,
            sample
        )
    } else {
        format!("Found {} result(s): {}.", rows.len(), sample)
    }
}

pub(crate) fn render_rows_compact_summary(rows: &[serde_json::Value]) -> String {
    render_rows_summary(rows, 3)
}

pub(crate) fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let r = 6371.0_f64;
    let dlat = (lat2 - lat1).to_radians();
    let dlon = (lon2 - lon1).to_radians();
    let a = (dlat / 2.0).sin().powi(2)
        + lat1.to_radians().cos() * lat2.to_radians().cos() * (dlon / 2.0).sin().powi(2);
    let c = 2.0 * a.sqrt().atan2((1.0 - a).sqrt());
    r * c
}

fn extract_point(
    row: &serde_json::Value,
    location_hints: Option<&LocationFieldHints>,
) -> Option<(f64, f64)> {
    fn extract_from_object(
        obj: &serde_json::Map<String, serde_json::Value>,
        location_hints: Option<&LocationFieldHints>,
        inside_geo_object: bool,
    ) -> Option<(f64, f64)> {
        let mut lat_val = None;
        let mut lon_val = None;
        let hints = location_hints?;
        for (k, v) in obj {
            let lower = k.to_lowercase();
            let is_lat = hints.latitude_keys.contains(&lower)
                || (inside_geo_object && hints.geo_latitude_member_keys.contains(&lower));
            let is_lon = hints.longitude_keys.contains(&lower)
                || (inside_geo_object && hints.geo_longitude_member_keys.contains(&lower));
            if is_lat && lat_val.is_none() {
                lat_val = v.as_f64();
            } else if is_lon && lon_val.is_none() {
                lon_val = v.as_f64();
            }
        }
        lat_val.zip(lon_val)
    }

    fn walk(
        v: &serde_json::Value,
        location_hints: Option<&LocationFieldHints>,
        inside_geo_object: bool,
    ) -> Option<(f64, f64)> {
        match v {
            serde_json::Value::Object(obj) => {
                if let Some(p) = extract_from_object(obj, location_hints, inside_geo_object) {
                    return Some(p);
                }
                if let Some(hints) = location_hints {
                    let mut hinted_keys = hints.geo_object_keys.iter().cloned().collect::<Vec<_>>();
                    hinted_keys.sort();
                    for key in hinted_keys {
                        if let Some(nested) = obj.get(&key)
                            && let Some(p) = walk(nested, location_hints, true)
                        {
                            return Some(p);
                        }
                    }
                }
                for nested in obj.values() {
                    if let Some(p) = walk(nested, location_hints, inside_geo_object) {
                        return Some(p);
                    }
                }
                None
            }
            serde_json::Value::Array(items) => items
                .iter()
                .find_map(|item| walk(item, location_hints, inside_geo_object)),
            _ => None,
        }
    }

    walk(row, location_hints, false)
}

fn compute_metric_value(rows: &[serde_json::Value], metric: &MetricSpec) -> Option<f64> {
    let metric_output_key = metric.output_key();
    if let Some(value) = rows.iter().find_map(|row| {
        row.get(&metric_output_key)
            .and_then(serde_json::Value::as_f64)
    }) {
        return Some(value);
    }

    fn agg_stats_for_rows(
        rows: &[serde_json::Value],
        aggs: &[AggFunc],
    ) -> std::collections::HashMap<AggFunc, MetricStats> {
        let mut stats_map = std::collections::HashMap::new();
        for agg in aggs {
            stats_map.insert(agg.clone(), MetricStats::default());
        }
        for row in rows {
            for agg in aggs {
                if let Some(stats) = stats_map.get_mut(agg) {
                    match agg.op {
                        AggOp::Count => {
                            if let Some(field) = agg.field.as_deref() {
                                if let Some(value) = value_at_path(row, field)
                                    && !value.is_null()
                                {
                                    stats.count += 1;
                                }
                            } else {
                                stats.count += 1;
                            }
                        }
                        AggOp::Sum | AggOp::Avg | AggOp::Min | AggOp::Max | AggOp::Stddev => {
                            let Some(field) = agg.field.as_deref() else {
                                continue;
                            };
                            if let Some(v) = value_at_path(row, field).and_then(|v| v.as_f64()) {
                                stats.observe(v);
                            }
                        }
                    }
                }
            }
        }
        stats_map
    }

    fn agg_value(agg: &AggFunc, stats: &MetricStats) -> Option<f64> {
        match agg.op {
            AggOp::Count => Some(stats.count as f64),
            AggOp::Sum => (stats.count > 0).then_some(stats.sum),
            AggOp::Avg => (stats.count > 0).then_some(stats.sum / stats.count as f64),
            AggOp::Min => stats.min,
            AggOp::Max => stats.max,
            AggOp::Stddev => {
                if stats.count > 1 {
                    let mean = stats.sum / stats.count as f64;
                    let variance = (stats.sum_sq / stats.count as f64) - mean * mean;
                    Some(variance.max(0.0).sqrt())
                } else if stats.count == 1 {
                    Some(0.0)
                } else {
                    None
                }
            }
        }
    }

    match metric {
        MetricSpec::Count => Some(rows.len() as f64),
        MetricSpec::Ref { .. } => None,
        MetricSpec::Formula { expr, .. } => {
            let parsed = parse_metric_formula(expr).ok()?;
            let aggs = collect_agg_funcs(&parsed);
            let stats = agg_stats_for_rows(rows, &aggs);
            let lookup = |agg: &AggFunc| stats.get(agg).and_then(|s| agg_value(agg, s));
            eval_metric_expr(&parsed, lookup)
        }
        MetricSpec::Sum { field }
        | MetricSpec::Avg { field }
        | MetricSpec::Min { field }
        | MetricSpec::Max { field }
        | MetricSpec::Stddev { field } => {
            let mut stats = MetricStats::default();
            for row in rows {
                if let Some(v) = value_at_path(row, field).and_then(|v| v.as_f64()) {
                    stats.observe(v);
                }
            }
            match metric {
                MetricSpec::Sum { .. } => (stats.count > 0).then_some(stats.sum),
                MetricSpec::Avg { .. } => {
                    (stats.count > 0).then_some(stats.sum / stats.count as f64)
                }
                MetricSpec::Min { .. } => stats.min,
                MetricSpec::Max { .. } => stats.max,
                MetricSpec::Stddev { .. } => {
                    if stats.count > 1 {
                        let mean = stats.sum / stats.count as f64;
                        let variance = (stats.sum_sq / stats.count as f64) - mean * mean;
                        Some(variance.max(0.0).sqrt())
                    } else if stats.count == 1 {
                        Some(0.0)
                    } else {
                        None
                    }
                }
                MetricSpec::Count => Some(rows.len() as f64),
                MetricSpec::Ref { .. } => None,
                MetricSpec::Formula { .. } => None,
            }
        }
    }
}

fn parse_time_millis(value: &serde_json::Value) -> Option<i64> {
    if let Some(n) = value.as_i64() {
        return Some(n);
    }
    let s = value.as_str()?;
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

fn pick_time_field(
    rows: &[serde_json::Value],
    field_hints: Option<&RecordFieldHints>,
) -> Option<String> {
    let mut stats: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for row in rows {
        let Some(obj) = row.as_object() else {
            continue;
        };
        for (k, v) in obj {
            if parse_time_millis(v).is_some() {
                *stats.entry(k.clone()).or_insert(0) += 1;
            }
        }
    }
    stats
        .into_iter()
        .max_by_key(|(k, c)| {
            let schema_bonus =
                if field_hints.is_some_and(|h| contains_field_hint(Some(&h.time_keys), k)) {
                    300
                } else {
                    0
                };
            (*c as i64) * 1000 + schema_bonus
        })
        .map(|(k, _)| k)
}

fn merge_primitive_fields(
    out: &mut serde_json::Map<String, serde_json::Value>,
    row: &serde_json::Value,
    prefix: &str,
    limit: usize,
) {
    let Some(obj) = row.as_object() else {
        return;
    };
    let mut keys = obj.keys().cloned().collect::<Vec<_>>();
    keys.sort();
    let mut added = 0usize;
    for k in keys {
        if added >= limit {
            break;
        }
        let Some(v) = obj.get(&k) else {
            continue;
        };
        match v {
            serde_json::Value::String(_)
            | serde_json::Value::Number(_)
            | serde_json::Value::Bool(_) => {
                out.insert(format!("{prefix}.{k}"), v.clone());
                added += 1;
            }
            _ => {}
        }
    }
}

pub(crate) fn compare_rows(
    left_source: &str,
    right_source: &str,
    metric: &MetricSpec,
    left_rows: &[serde_json::Value],
    right_rows: &[serde_json::Value],
) -> Vec<serde_json::Value> {
    let mut out = serde_json::Map::new();
    out.insert("left_source".to_string(), serde_json::json!(left_source));
    out.insert("right_source".to_string(), serde_json::json!(right_source));
    out.insert("left_rows".to_string(), serde_json::json!(left_rows.len()));
    out.insert(
        "right_rows".to_string(),
        serde_json::json!(right_rows.len()),
    );
    out.insert("metric".to_string(), serde_json::json!(metric.to_string()));

    let left_value = compute_metric_value(left_rows, metric);
    let right_value = compute_metric_value(right_rows, metric);
    if let Some(v) = left_value {
        out.insert("left_value".to_string(), serde_json::json!(v));
    }
    if let Some(v) = right_value {
        out.insert("right_value".to_string(), serde_json::json!(v));
    }
    if let (Some(l), Some(r)) = (left_value, right_value) {
        out.insert("delta".to_string(), serde_json::json!(l - r));
    } else {
        out.insert(
            "compare_error".to_string(),
            serde_json::json!("metric_values_missing"),
        );
        out.insert(
            "message".to_string(),
            serde_json::json!("Metric values could not be computed for one or both sources."),
        );
    }
    vec![serde_json::Value::Object(out)]
}

pub(crate) fn join_on_time_rows(
    left_rows: &[serde_json::Value],
    right_rows: &[serde_json::Value],
    left_time_field: Option<&str>,
    right_time_field: Option<&str>,
    window_minutes: Option<i64>,
    field_hints: Option<&RecordFieldHints>,
) -> Vec<serde_json::Value> {
    let left_tf = left_time_field
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(std::string::ToString::to_string)
        .or_else(|| pick_time_field(left_rows, field_hints));
    let right_tf = right_time_field
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(std::string::ToString::to_string)
        .or_else(|| pick_time_field(right_rows, field_hints));

    let Some(left_tf) = left_tf else {
        return vec![serde_json::json!({
            "join_status": "missing_left_time_field",
            "left_rows": left_rows.len(),
            "right_rows": right_rows.len()
        })];
    };
    let Some(right_tf) = right_tf else {
        return vec![serde_json::json!({
            "join_status": "missing_right_time_field",
            "left_rows": left_rows.len(),
            "right_rows": right_rows.len()
        })];
    };

    let window_ms = window_minutes.unwrap_or(0).max(0) * 60_000;
    let right_index = right_rows
        .iter()
        .filter_map(|r| {
            let ts = value_at_path(r, &right_tf)
                .as_ref()
                .and_then(parse_time_millis)?;
            Some((ts, r))
        })
        .collect::<Vec<_>>();

    let mut out = Vec::new();
    for left in left_rows {
        let Some(left_ts) = value_at_path(left, &left_tf)
            .as_ref()
            .and_then(parse_time_millis)
        else {
            continue;
        };
        let nearest = right_index
            .iter()
            .filter_map(|(right_ts, right_row)| {
                let delta = (right_ts - left_ts).abs();
                let within = if window_ms == 0 {
                    delta == 0
                } else {
                    delta <= window_ms
                };
                if within {
                    Some((delta, *right_ts, *right_row))
                } else {
                    None
                }
            })
            .min_by_key(|(delta, _, _)| *delta);

        if let Some((delta_ms, right_ts, right_row)) = nearest {
            let mut joined = serde_json::Map::new();
            joined.insert("left_time_field".to_string(), serde_json::json!(left_tf));
            joined.insert("right_time_field".to_string(), serde_json::json!(right_tf));
            joined.insert(
                "window_minutes".to_string(),
                serde_json::json!(window_ms / 60_000),
            );
            joined.insert("left_time".to_string(), serde_json::json!(left_ts));
            joined.insert("right_time".to_string(), serde_json::json!(right_ts));
            joined.insert(
                "time_delta_seconds".to_string(),
                serde_json::json!(delta_ms / 1000),
            );
            merge_primitive_fields(&mut joined, left, "left", 10);
            merge_primitive_fields(&mut joined, right_row, "right", 10);
            out.push(serde_json::Value::Object(joined));
        }
    }

    if out.is_empty() {
        return vec![serde_json::json!({
            "join_status": "no_matches",
            "left_time_field": left_tf,
            "right_time_field": right_tf,
            "window_minutes": window_ms / 60_000,
            "left_rows": left_rows.len(),
            "right_rows": right_rows.len()
        })];
    }

    if out.len() > 200 {
        out.truncate(200);
    }
    out
}

pub(crate) fn threshold_check_rows(
    rows: &[serde_json::Value],
    field: &str,
    operator: &str,
    value: f64,
) -> Vec<serde_json::Value> {
    let op = operator.trim();
    let mut evaluated = 0usize;
    let mut matched = 0usize;
    let mut sample_matches = Vec::new();
    let mut sample_fails = Vec::new();

    for row in rows {
        let Some(n) = value_at_path(row, field).and_then(|v| v.as_f64()) else {
            continue;
        };
        evaluated += 1;
        let ok = match op {
            ">" => n > value,
            ">=" => n >= value,
            "<" => n < value,
            "<=" => n <= value,
            "=" | "==" => (n - value).abs() < f64::EPSILON,
            "!=" => (n - value).abs() >= f64::EPSILON,
            _ => false,
        };
        if ok {
            matched += 1;
            if sample_matches.len() < 3 {
                sample_matches.push(serde_json::json!(n));
            }
        } else if sample_fails.len() < 3 {
            sample_fails.push(serde_json::json!(n));
        }
    }

    let failed = evaluated.saturating_sub(matched);
    let match_rate = if evaluated == 0 {
        0.0
    } else {
        matched as f64 / evaluated as f64
    };
    let status = if evaluated == 0 {
        "no_numeric_values"
    } else if failed == 0 {
        "pass"
    } else {
        "fail"
    };

    vec![serde_json::json!({
        "field": field,
        "operator": op,
        "threshold": value,
        "source_rows": rows.len(),
        "evaluated_rows": evaluated,
        "matched_rows": matched,
        "failed_rows": failed,
        "match_rate": match_rate,
        "status": status,
        "sample_matches": sample_matches,
        "sample_failures": sample_fails
    })]
}

fn parse_time_sort_key(value: &serde_json::Value) -> Option<i64> {
    match value {
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
        serde_json::Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|v| v as i64)),
        _ => None,
    }
}

pub(crate) fn summarize_trend_rows(
    rows: &[serde_json::Value],
    time_field: &str,
    value_field: &str,
) -> Vec<serde_json::Value> {
    let mut points = rows
        .iter()
        .filter_map(|row| {
            let time_value = value_at_path(row, time_field)?;
            let time_key = parse_time_sort_key(&time_value)?;
            let value = value_at_path(row, value_field)?.as_f64()?;
            Some((time_key, time_value, value))
        })
        .collect::<Vec<_>>();
    points.sort_by_key(|(time_key, _, _)| *time_key);
    if points.is_empty() {
        return vec![serde_json::json!({
            "trend_error": "no_time_series_points",
            "message": format!(
                "No rows contained both `{time_field}` and numeric `{value_field}` values for trend analysis."
            )
        })];
    }

    let first = points
        .first()
        .cloned()
        .unwrap_or((0, serde_json::Value::Null, 0.0));
    let last = points
        .last()
        .cloned()
        .unwrap_or((0, serde_json::Value::Null, 0.0));
    let min_value = points
        .iter()
        .map(|(_, _, value)| *value)
        .fold(f64::INFINITY, f64::min);
    let max_value = points
        .iter()
        .map(|(_, _, value)| *value)
        .fold(f64::NEG_INFINITY, f64::max);
    let avg_value = points.iter().map(|(_, _, value)| *value).sum::<f64>() / points.len() as f64;
    let delta = last.2 - first.2;
    let amplitude = max_value - min_value;
    let direction = if delta.abs() <= avg_value.abs().max(1.0) * 0.05 {
        "stable"
    } else if delta > 0.0 {
        "increasing"
    } else {
        "decreasing"
    };
    let pattern = if amplitude > avg_value.abs().max(1.0) * 0.25 {
        "fluctuating"
    } else {
        "steady"
    };

    vec![serde_json::json!({
        "time_field": time_field,
        "value_field": value_field,
        "point_count": points.len(),
        "start_time": first.1,
        "end_time": last.1,
        "start_value": first.2,
        "end_value": last.2,
        "delta": delta,
        "min_value": min_value,
        "max_value": max_value,
        "avg_value": avg_value,
        "trend_direction": direction,
        "trend_pattern": pattern
    })]
}

pub(crate) fn render_trend_summary(rows: &[serde_json::Value]) -> String {
    let Some(row) = rows.first() else {
        return "No trend summary available.".to_string();
    };
    if let Some(message) = row.get("message").and_then(|v| v.as_str()) {
        return message.to_string();
    }
    let value_field = row
        .get("value_field")
        .and_then(|v| v.as_str())
        .unwrap_or("value");
    let direction = row
        .get("trend_direction")
        .and_then(|v| v.as_str())
        .unwrap_or("stable");
    let pattern = row
        .get("trend_pattern")
        .and_then(|v| v.as_str())
        .unwrap_or("steady");
    let start_time = row.get("start_time").map_or_else(
        || "start".to_string(),
        |v| v.to_string().trim_matches('"').to_string(),
    );
    let end_time = row.get("end_time").map_or_else(
        || "end".to_string(),
        |v| v.to_string().trim_matches('"').to_string(),
    );
    let start_value = row
        .get("start_value")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let end_value = row.get("end_value").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let min_value = row.get("min_value").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let max_value = row.get("max_value").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let point_count = row.get("point_count").and_then(|v| v.as_u64()).unwrap_or(0);

    format!(
        "{value_field} shows a {direction} and {pattern} trend from {start_time} to {end_time} across {point_count} points, moving from {start_value:.2} to {end_value:.2} with a range of {min_value:.2} to {max_value:.2}."
    )
}

pub(crate) fn compute_distance_rows(
    vessel_rows: &[serde_json::Value],
    target_rows: &[serde_json::Value],
    location_hints: Option<&LocationFieldHints>,
    field_hints: Option<&RecordFieldHints>,
) -> Vec<serde_json::Value> {
    fn primitive_as_string(v: &serde_json::Value) -> Option<String> {
        match v {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Number(n) => Some(n.to_string()),
            serde_json::Value::Bool(b) => Some(b.to_string()),
            _ => None,
        }
    }

    fn looks_like_date_prefix(s: &str) -> bool {
        let b = s.as_bytes();
        b.len() >= 10
            && b[0].is_ascii_digit()
            && b[1].is_ascii_digit()
            && b[2].is_ascii_digit()
            && b[3].is_ascii_digit()
            && b[4] == b'-'
            && b[5].is_ascii_digit()
            && b[6].is_ascii_digit()
            && b[7] == b'-'
            && b[8].is_ascii_digit()
            && b[9].is_ascii_digit()
    }

    fn timestamp_rank(s: &str) -> (i32, i64, String) {
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
            return (2, dt.timestamp_millis(), String::new());
        }
        if looks_like_date_prefix(s) {
            return (1, 0, s.to_string());
        }
        (0, 0, s.to_string())
    }

    fn pick_timestamp_field(
        rows: &[serde_json::Value],
        field_hints: Option<&RecordFieldHints>,
    ) -> Option<String> {
        let mut stats: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        for row in rows {
            let Some(obj) = row.as_object() else {
                continue;
            };
            for (k, v) in obj {
                let Some(s) = v.as_str() else {
                    continue;
                };
                if timestamp_rank(s).0 <= 0 {
                    continue;
                }
                *stats.entry(k.clone()).or_insert(0) += 1;
            }
        }
        stats
            .into_iter()
            .max_by_key(|(k, count)| {
                let schema_bonus =
                    if field_hints.is_some_and(|h| contains_field_hint(Some(&h.time_keys), k)) {
                        300
                    } else {
                        0
                    };
                (*count as i64) * 1000 + schema_bonus
            })
            .map(|(k, _)| k)
    }

    fn pick_vessel_key_field(
        rows: &[serde_json::Value],
        timestamp_field: Option<&str>,
        field_hints: Option<&RecordFieldHints>,
    ) -> Option<String> {
        let mut stats: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        for row in rows {
            let Some(obj) = row.as_object() else {
                continue;
            };
            for (k, v) in obj {
                if timestamp_field.is_some_and(|ts| ts == k) {
                    continue;
                }
                if field_hints.is_some_and(|h| contains_field_hint(Some(&h.time_keys), k)) {
                    continue;
                }
                if primitive_as_string(v).is_none() {
                    continue;
                }
                *stats.entry(k.clone()).or_insert(0) += 1;
            }
        }
        stats
            .into_iter()
            .max_by_key(|(k, count)| {
                let key_bonus = if field_hints
                    .is_some_and(|h| contains_field_hint(Some(&h.identifier_keys), k))
                {
                    400
                } else if field_hints.is_some_and(|h| contains_field_hint(Some(&h.label_keys), k)) {
                    100
                } else {
                    0
                };
                (*count as i64) * 1000 + key_bonus
            })
            .map(|(k, _)| k)
    }

    fn latest_rows_for_distance(
        rows: &[serde_json::Value],
        field_hints: Option<&RecordFieldHints>,
    ) -> Vec<serde_json::Value> {
        let Some(ts_field) = pick_timestamp_field(rows, field_hints) else {
            return rows.to_vec();
        };
        let key_field = pick_vessel_key_field(rows, Some(&ts_field), field_hints);

        if let Some(key_field) = key_field {
            let mut latest: std::collections::BTreeMap<String, (String, serde_json::Value)> =
                std::collections::BTreeMap::new();
            for row in rows {
                let Some(obj) = row.as_object() else {
                    continue;
                };
                let Some(group_key) = obj.get(&key_field).and_then(primitive_as_string) else {
                    continue;
                };
                let Some(ts) = obj
                    .get(&ts_field)
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                else {
                    continue;
                };
                let replace = latest
                    .get(&group_key)
                    .is_none_or(|(cur_ts, _)| timestamp_rank(&ts) > timestamp_rank(cur_ts));
                if replace {
                    latest.insert(group_key, (ts, row.clone()));
                }
            }
            if !latest.is_empty() {
                return latest.into_values().map(|(_, row)| row).collect();
            }
        }

        let best = rows
            .iter()
            .filter_map(|row| {
                let obj = row.as_object()?;
                let ts = obj.get(&ts_field).and_then(|v| v.as_str())?;
                Some((timestamp_rank(ts), row.clone()))
            })
            .max_by(|(a, _), (b, _)| a.cmp(b))
            .map(|(_, row)| row);
        best.into_iter().collect()
    }

    fn copy_selected_scalars(
        out: &mut serde_json::Map<String, serde_json::Value>,
        row: &serde_json::Value,
        field_hints: Option<&RecordFieldHints>,
        prefix: Option<&str>,
    ) {
        let Some(obj) = row.as_object() else {
            return;
        };
        let mut keys = obj.keys().cloned().collect::<Vec<_>>();
        keys.sort();
        for k in keys {
            if let Some(hints) = field_hints
                && !(contains_field_hint(Some(&hints.label_keys), &k)
                    || contains_field_hint(Some(&hints.identifier_keys), &k))
            {
                continue;
            }
            let Some(v) = obj.get(&k) else {
                continue;
            };
            match v {
                serde_json::Value::String(_)
                | serde_json::Value::Number(_)
                | serde_json::Value::Bool(_) => {
                    let key = prefix
                        .map(|p| format!("{p}{k}"))
                        .unwrap_or_else(|| k.clone());
                    out.insert(key, v.clone());
                }
                _ => {}
            }
        }
    }

    let target_points = target_rows
        .iter()
        .filter_map(|row| extract_point(row, location_hints).map(|point| (row, point)))
        .collect::<Vec<_>>();
    if target_points.is_empty() {
        return Vec::new();
    }

    let latest_rows = latest_rows_for_distance(vessel_rows, field_hints);
    let mut out = latest_rows
        .iter()
        .flat_map(|r| {
            let (lat, lon) = extract_point(r, location_hints)?;
            Some(
                target_points
                    .iter()
                    .map(|(target_row, (t_lat, t_lon))| {
                        let km = haversine_km(lat, lon, *t_lat, *t_lon);
                        let mut row = serde_json::Map::new();
                        copy_selected_scalars(&mut row, r, field_hints, None);
                        copy_selected_scalars(&mut row, target_row, field_hints, Some("target_"));
                        row.insert("distanceKm".to_string(), serde_json::json!(km));
                        serde_json::Value::Object(row)
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .flatten()
        .collect::<Vec<_>>();

    out.sort_by(|a, b| {
        let av = a
            .get("distanceKm")
            .and_then(|v| v.as_f64())
            .unwrap_or(f64::INFINITY);
        let bv = b
            .get("distanceKm")
            .and_then(|v| v.as_f64())
            .unwrap_or(f64::INFINITY);
        av.partial_cmp(&bv).unwrap_or(std::cmp::Ordering::Equal)
    });
    if out.len() > 10 {
        out.truncate(10);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_distance_rows_uses_schema_location_hints() {
        let vessel_rows = vec![serde_json::json!({
            "name": "v1",
            "xCoord": 53.216406,
            "yCoord": 1.806167
        })];
        let target_rows = vec![serde_json::json!({
            "location": { "xCoord": 53.2105, "yCoord": 1.8095 }
        })];
        let hints = LocationFieldHints::from_schema_lists(
            &["xCoord".to_string()],
            &["yCoord".to_string()],
            &["location".to_string()],
        );
        let out = compute_distance_rows(&vessel_rows, &target_rows, Some(&hints), None);
        assert!(
            !out.is_empty(),
            "expected distance rows from hinted location fields"
        );
        assert!(out[0].get("distanceKm").and_then(|v| v.as_f64()).is_some());
    }

    #[test]
    fn compute_distance_rows_requires_location_hints_for_coordinate_keys() {
        let vessel_rows = vec![serde_json::json!({
            "name": "v1",
            "lat": 53.216406,
            "lon": 1.806167
        })];
        let target_rows = vec![serde_json::json!({
            "location": { "point": { "latitude": 53.2105, "longitude": 1.8095 } }
        })];

        let out = compute_distance_rows(&vessel_rows, &target_rows, None, None);
        assert!(
            out.is_empty(),
            "distance extraction should come from DomainConfig/SLS hints, not built-in lat/lon fallback"
        );
    }

    #[test]
    fn compute_distance_rows_uses_geo_object_role_for_nested_coordinates() {
        let vessel_rows = vec![serde_json::json!({
            "mmsi": 123456789,
            "name": "the wagon",
            "lat": 53.228934,
            "lon": 1.822029,
            "messageTimestamp": "2026-02-23T11:00:00Z"
        })];
        let target_rows = vec![serde_json::json!({
            "name": "Turbine 2",
            "shortName": "T3",
            "location": {
                "point": {
                    "latitude": 53.2105,
                    "longitude": 1.8095
                }
            }
        })];
        let hints = LocationFieldHints::from_schema_lists(
            &["lat".to_string(), "location".to_string()],
            &["lon".to_string(), "location".to_string()],
            &["location".to_string()],
        );

        let out = compute_distance_rows(&vessel_rows, &target_rows, Some(&hints), None);

        assert_eq!(
            out.len(),
            1,
            "expected the hinted geo object to produce a distance row"
        );
        assert!(out[0].get("distanceKm").and_then(|v| v.as_f64()).is_some());
    }

    #[test]
    fn compute_distance_rows_uses_latest_position_per_vessel() {
        let vessel_rows = vec![
            serde_json::json!({
                "mmsi": 111111111,
                "name": "v1",
                "lat": 53.0000,
                "lon": 1.0000,
                "messageTimestamp": "2026-02-16T00:00:00Z"
            }),
            serde_json::json!({
                "mmsi": 111111111,
                "name": "v1",
                "lat": 53.2105,
                "lon": 1.8095,
                "messageTimestamp": "2026-02-16T00:10:00Z"
            }),
        ];
        let target_rows = vec![serde_json::json!({
            "location": { "point": { "latitude": 53.2105, "longitude": 1.8095 } }
        })];

        let hints = LocationFieldHints::from_schema_lists(
            &["lat".to_string(), "latitude".to_string()],
            &["lon".to_string(), "longitude".to_string()],
            &["location".to_string(), "point".to_string()],
        );
        let out = compute_distance_rows(&vessel_rows, &target_rows, Some(&hints), None);
        assert_eq!(out.len(), 1, "expected one latest row for the same vessel");
        let d = out[0]
            .get("distanceKm")
            .and_then(|v| v.as_f64())
            .unwrap_or(9999.0);
        assert!(d < 0.02, "latest point should be near target, got {d}");
    }

    #[test]
    fn compute_distance_rows_carries_target_identity_and_ranks_nearest() {
        let vessel_rows = vec![serde_json::json!({
            "mmsi": 123456789,
            "name": "the wagon",
            "lat": 53.216406,
            "lon": 1.806167,
            "messageTimestamp": "2026-02-23T11:00:00Z"
        })];
        let target_rows = vec![
            serde_json::json!({
                "name": "Turbine 1",
                "shortName": "T1",
                "location": { "point": { "latitude": 53.2, "longitude": 1.8 } }
            }),
            serde_json::json!({
                "name": "Turbine 12",
                "shortName": "T102",
                "location": { "point": { "latitude": 53.3155, "longitude": 1.9045 } }
            }),
        ];

        let hints = LocationFieldHints::from_schema_lists(
            &["lat".to_string(), "latitude".to_string()],
            &["lon".to_string(), "longitude".to_string()],
            &["location".to_string(), "point".to_string()],
        );
        let out = compute_distance_rows(&vessel_rows, &target_rows, Some(&hints), None);
        assert_eq!(out.len(), 2, "expected one distance row per target");
        assert_eq!(
            out[0].get("target_name").and_then(|v| v.as_str()),
            Some("Turbine 1")
        );
        assert_eq!(
            out[0].get("target_shortName").and_then(|v| v.as_str()),
            Some("T1")
        );
        let first_distance = out[0]
            .get("distanceKm")
            .and_then(|v| v.as_f64())
            .unwrap_or(f64::INFINITY);
        let second_distance = out[1]
            .get("distanceKm")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        assert!(
            first_distance < second_distance,
            "expected nearest target first, got {first_distance} >= {second_distance}"
        );
    }

    #[test]
    fn join_on_time_rows_matches_nearest_within_window() {
        let left = vec![
            serde_json::json!({"id": "l1", "timestamp": "2026-02-16T00:00:00Z", "value": 10.0}),
            serde_json::json!({"id": "l2", "timestamp": "2026-02-16T00:20:00Z", "value": 20.0}),
        ];
        let right = vec![
            serde_json::json!({"id": "r1", "time": "2026-02-16T00:05:00Z", "value": 11.0}),
            serde_json::json!({"id": "r2", "time": "2026-02-16T00:21:00Z", "value": 19.0}),
        ];
        let out = join_on_time_rows(
            &left,
            &right,
            Some("timestamp"),
            Some("time"),
            Some(10),
            None,
        );
        assert!(!out.is_empty(), "expected matched rows");
        assert!(
            out.iter().any(|r| r
                .get("time_delta_seconds")
                .and_then(|v| v.as_i64())
                .unwrap_or(9999)
                <= 600),
            "expected at least one join within 10 minutes"
        );
    }

    #[test]
    fn filter_rows_contains_matches_case_insensitively() {
        let rows = vec![
            serde_json::json!({"shortName": "AB-01", "name": "Alpha Bay"}),
            serde_json::json!({"shortName": "zz-02", "name": "Zulu Zone"}),
        ];
        let out = filter_rows(&rows, "shortName", "contains", &serde_json::json!("a"));
        assert_eq!(out.len(), 1, "expected one matching row");
        assert_eq!(
            out[0].get("shortName").and_then(|v| v.as_str()),
            Some("AB-01")
        );
    }

    #[test]
    fn aggregate_metrics_averages_values_across_nested_arrays() {
        let rows = vec![
            serde_json::json!({
                "name": "A",
                "children": [
                    {"value": 10.0},
                    {"value": 20.0}
                ]
            }),
            serde_json::json!({
                "name": "B",
                "children": [
                    {"value": 40.0}
                ]
            }),
        ];

        let out = aggregate_metrics(
            &rows,
            &[],
            &[MetricSpec::Avg {
                field: "children.value".to_string(),
            }],
        );

        let avg = out[0]
            .get("avg_children_value")
            .and_then(|value| value.as_f64())
            .expect("nested average");
        assert!(
            (avg - 23.333333333333332).abs() < 0.0000001,
            "expected nested values to be averaged, got {avg}"
        );
    }

    #[test]
    fn render_rows_summary_prioritizes_short_name_and_name() {
        let rows = vec![serde_json::json!({
            "commercialDateTimeOfOperation": "2022-09-26T10:00:38.526Z",
            "name": "Wind Farm 1",
            "plantId": "PLANT-  1",
            "ratedCapacity": 424.7366092786337,
            "shortName": "WF1"
        })];
        let out = render_rows_summary(&rows, 1);
        assert!(out.contains("name: Wind Farm 1"));
        assert!(out.contains("shortName: WF1"));
    }

    #[test]
    fn render_rows_summary_labels_preview_when_limited() {
        let rows = vec![
            serde_json::json!({"name": "A"}),
            serde_json::json!({"name": "B"}),
            serde_json::json!({"name": "C"}),
        ];
        let out = render_rows_summary(&rows, 2);
        assert!(out.starts_with("Found 3 result(s), showing first 2:"));
        assert!(out.contains("name: A"));
        assert!(out.contains("name: B"));
        assert!(!out.contains("name: C"));
    }

    #[test]
    fn threshold_check_rows_reports_counts() {
        let rows = vec![
            serde_json::json!({"distanceKm": 0.4}),
            serde_json::json!({"distanceKm": 1.2}),
            serde_json::json!({"distanceKm": 0.8}),
        ];
        let out = threshold_check_rows(&rows, "distanceKm", "<=", 1.0);
        assert_eq!(out.len(), 1);
        let obj = out[0].as_object().expect("summary object");
        assert_eq!(obj.get("evaluated_rows").and_then(|v| v.as_u64()), Some(3));
        assert_eq!(obj.get("matched_rows").and_then(|v| v.as_u64()), Some(2));
        assert_eq!(obj.get("failed_rows").and_then(|v| v.as_u64()), Some(1));
    }

    #[test]
    fn summarize_trend_rows_detects_increasing_trend() {
        let rows = vec![
            serde_json::json!({"time": "2026-02-10", "windSpeed10m": 4.0}),
            serde_json::json!({"time": "2026-02-11", "windSpeed10m": 5.5}),
            serde_json::json!({"time": "2026-02-12", "windSpeed10m": 6.0}),
        ];
        let out = summarize_trend_rows(&rows, "time", "windSpeed10m");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["trend_direction"], "increasing");
        let summary = render_trend_summary(&out);
        assert!(summary.contains("windSpeed10m"));
        assert!(summary.contains("increasing"));
    }

    #[test]
    fn ranked_count_summary_is_generic_and_field_driven() {
        let rows = vec![
            serde_json::json!({"siteCode": "WF-01", "severity": "HIGH", "count": 7}),
            serde_json::json!({"siteCode": "WF-02", "severity": "MEDIUM", "count": 3}),
        ];
        let summary = render_ranked_count_summary(&rows);
        assert!(
            summary.contains("Highest counts by severity, siteCode")
                || summary.contains("Highest counts by siteCode, severity"),
            "expected generic grouped summary, got {summary}"
        );
        assert!(
            summary.contains("siteCode=WF-01") && summary.contains("severity=HIGH"),
            "expected summary to use row fields directly, got {summary}"
        );
        assert!(
            !summary.contains("Turbines with most alarms"),
            "unexpected domain hardcoding in summary: {summary}"
        );
    }

    #[test]
    fn aggregate_metrics_supports_numeric_rollups() {
        let rows = vec![
            serde_json::json!({"site": "A", "value": 1.0}),
            serde_json::json!({"site": "A", "value": 3.0}),
            serde_json::json!({"site": "B", "value": 2.0}),
        ];
        let metrics = vec![
            MetricSpec::Count,
            MetricSpec::Sum {
                field: "value".to_string(),
            },
            MetricSpec::Avg {
                field: "value".to_string(),
            },
            MetricSpec::Min {
                field: "value".to_string(),
            },
            MetricSpec::Max {
                field: "value".to_string(),
            },
            MetricSpec::Stddev {
                field: "value".to_string(),
            },
        ];
        let out = aggregate_metrics(&rows, &["site".to_string()], &metrics);
        assert_eq!(out.len(), 2);
        let a_row = out
            .iter()
            .find(|r| r.get("site").and_then(|v| v.as_str()) == Some("A"))
            .expect("site A row");
        assert_eq!(a_row.get("count").and_then(|v| v.as_i64()), Some(2));
        assert_eq!(a_row.get("sum_value").and_then(|v| v.as_f64()), Some(4.0));
        assert_eq!(a_row.get("avg_value").and_then(|v| v.as_f64()), Some(2.0));
        assert_eq!(a_row.get("min_value").and_then(|v| v.as_f64()), Some(1.0));
        assert_eq!(a_row.get("max_value").and_then(|v| v.as_f64()), Some(3.0));
        assert_eq!(
            a_row.get("stddev_value").and_then(|v| v.as_f64()),
            Some(1.0)
        );
    }

    #[test]
    fn aggregate_metrics_supports_formula_metrics() {
        let rows = vec![
            serde_json::json!({"downtimeHours": 1.0, "totalHours": 4.0}),
            serde_json::json!({"downtimeHours": 1.0, "totalHours": 4.0}),
        ];
        let metrics = vec![MetricSpec::Formula {
            name: "availability".to_string(),
            expr: "1 - (sum(downtimeHours) / sum(totalHours))".to_string(),
        }];
        let out = aggregate_metrics(&rows, &[], &metrics);
        assert_eq!(out.len(), 1);
        let value = out[0]
            .get("metric_availability")
            .and_then(|v| v.as_f64())
            .unwrap_or(-1.0);
        assert!(
            (value - 0.75).abs() < 1e-6,
            "expected availability 0.75, got {value}"
        );
    }

    #[test]
    fn render_aggregate_summary_prefers_group_label_over_raw_uid() {
        let rows = vec![serde_json::json!({
            "partOfOffshoreWindFarmUid": "FARM-UID-  4",
            "partOfOffshoreWindFarmUid_label": "Wind Farm 4",
            "avg_accumulatedDowntime": 286.74717398821093
        })];

        let summary = render_aggregate_summary(&rows);

        assert!(summary.contains("partOfOffshoreWindFarmUid=Wind Farm 4"));
        assert!(!summary.contains("partOfOffshoreWindFarmUid=FARM-UID-  4"));
    }

    #[test]
    fn render_ranked_count_summary_prefers_group_label_over_raw_uid() {
        let rows = vec![serde_json::json!({
            "partOfOffshoreWindFarmUid": "FARM-UID-  4",
            "partOfOffshoreWindFarmUid_label": "Wind Farm 4",
            "count": 24
        })];

        let summary = render_ranked_count_summary(&rows);

        assert!(summary.contains("partOfOffshoreWindFarmUid=Wind Farm 4: 24"));
        assert!(!summary.contains("FARM-UID-  4"));
    }

    #[test]
    fn render_rows_summary_formats_nested_child_lists() {
        let rows = vec![serde_json::json!({
            "name": "Wind Farm 1",
            "shortName": "WF1",
            "hasOffshoreWindTurbine": [
                {"name": "Turbine 25", "shortName": "T25", "stringName": "STRING- 1"},
                {"name": "Turbine 49", "shortName": "T49", "stringName": "STRING- 1"}
            ]
        })];

        let summary = render_rows_summary(&rows, 20);

        assert!(summary.contains("Found 2 child item(s) in `hasOffshoreWindTurbine`"));
        assert!(summary.contains("1. name: Turbine 25"));
        assert!(summary.contains("stringName: STRING- 1"));
        assert!(!summary.contains("[{\""));
    }

    #[test]
    fn render_rows_summary_with_hints_uses_schema_roles_for_nested_child_lists() {
        let rows = vec![serde_json::json!({
            "name": "Wind Farm 1",
            "shortName": "WF1",
            "hasOffshoreWindTurbine": [
                {
                    "accumulatedDowntime": 237.68935408867463,
                    "name": "Turbine 25",
                    "shortName": "T25",
                    "stringName": "STRING- 1"
                },
                {
                    "accumulatedDowntime": 62.03138436999345,
                    "name": "Turbine 49",
                    "shortName": "T49",
                    "stringName": "STRING- 1"
                }
            ]
        })];
        let mut hints = RowsDisplayHints {
            parent_roles: RowDisplayRoles {
                label_fields: vec!["name".to_string()],
                entity_key_fields: vec!["shortName".to_string()],
                ..RowDisplayRoles::default()
            },
            ..RowsDisplayHints::default()
        };
        hints.relation_roles.insert(
            "hasOffshoreWindTurbine".to_string(),
            RowDisplayRoles {
                label_fields: vec!["name".to_string()],
                entity_key_fields: vec!["shortName".to_string()],
                numeric_fields: vec!["accumulatedDowntime".to_string()],
                ..RowDisplayRoles::default()
            },
        );
        hints.relation_record_types.insert(
            "hasOffshoreWindTurbine".to_string(),
            "OffshoreWindTurbine".to_string(),
        );

        let summary = render_rows_summary_with_hints(&rows, 20, &hints);

        assert!(summary.contains("Found 2 offshore wind turbine record(s) for Wind Farm 1 (WF1)"));
        assert!(summary.contains("1. Turbine 25 (T25) - accumulated downtime: 237.69"));
        assert!(summary.contains("2. Turbine 49 (T49) - accumulated downtime: 62.03"));
        assert!(!summary.contains("[{\""));
    }

    #[test]
    fn render_rows_summary_with_hints_avoids_json_parent_context_without_parent_scalars() {
        let rows = vec![serde_json::json!({
            "hasOffshoreWindTurbine": [
                {"name": "Turbine 25", "shortName": "T25"}
            ]
        })];
        let mut hints = RowsDisplayHints {
            parent_roles: RowDisplayRoles {
                label_fields: vec!["name".to_string()],
                entity_key_fields: vec!["shortName".to_string()],
                ..RowDisplayRoles::default()
            },
            ..RowsDisplayHints::default()
        };
        hints.relation_roles.insert(
            "hasOffshoreWindTurbine".to_string(),
            RowDisplayRoles {
                label_fields: vec!["name".to_string()],
                entity_key_fields: vec!["shortName".to_string()],
                ..RowDisplayRoles::default()
            },
        );
        hints.relation_record_types.insert(
            "hasOffshoreWindTurbine".to_string(),
            "OffshoreWindTurbine".to_string(),
        );

        let summary = render_rows_summary_with_hints(&rows, 20, &hints);

        assert!(summary.contains("for selected parent record"));
        assert!(summary.contains("1. Turbine 25 (T25)"));
        assert!(!summary.contains("{\"hasOffshoreWindTurbine\""));
    }

    #[test]
    fn compare_rows_uses_typed_metric() {
        let left = vec![
            serde_json::json!({"value": 1.0}),
            serde_json::json!({"value": 3.0}),
        ];
        let right = vec![serde_json::json!({"value": 2.0})];
        let metric = MetricSpec::Avg {
            field: "value".to_string(),
        };
        let out = compare_rows("left", "right", &metric, &left, &right);
        let obj = out[0].as_object().expect("compare result");
        assert_eq!(
            obj.get("metric").and_then(|v| v.as_str()),
            Some("avg(value)")
        );
        assert_eq!(obj.get("left_value").and_then(|v| v.as_f64()), Some(2.0));
        assert_eq!(obj.get("right_value").and_then(|v| v.as_f64()), Some(2.0));
        assert_eq!(obj.get("delta").and_then(|v| v.as_f64()), Some(0.0));
    }

    #[test]
    fn compare_rows_reads_preaggregated_metric_values() {
        let left = vec![serde_json::json!({"avg_accumulatedDowntime": 489.18018485188674})];
        let right = vec![serde_json::json!({"avg_accumulatedDowntime": 417.72387330791025})];
        let metric = MetricSpec::Avg {
            field: "accumulatedDowntime".to_string(),
        };
        let out = compare_rows("s2", "s4", &metric, &left, &right);
        let obj = out[0].as_object().expect("compare result");
        assert_eq!(
            obj.get("left_value").and_then(|v| v.as_f64()),
            Some(489.18018485188674)
        );
        assert_eq!(
            obj.get("right_value").and_then(|v| v.as_f64()),
            Some(417.72387330791025)
        );
        assert_eq!(
            obj.get("delta").and_then(|v| v.as_f64()),
            Some(489.18018485188674 - 417.72387330791025)
        );
        assert!(obj.get("compare_error").is_none());
    }
}
