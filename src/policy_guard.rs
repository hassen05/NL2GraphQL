use crate::planner::{PlanV2, PlanV2Op};
use crate::schema_registry::SchemaRegistry;
use crate::sls::Sls;

#[derive(Clone, Debug)]
pub(crate) enum PolicyFix {
    SetFirst { step_index: usize, value: i64 },
    CapFirst { step_index: usize, value: i64 },
    SetJoinLeftTimeField { step_index: usize, field: String },
    SetJoinRightTimeField { step_index: usize, field: String },
    SetJoinWindowMinutes { step_index: usize, value: i64 },
}

#[derive(Clone, Debug, Default)]
pub(crate) struct PolicyEvaluation {
    pub(crate) fixes: Vec<PolicyFix>,
    pub(crate) violations: Vec<String>,
}

fn normalize_token(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

fn selection_depth(fields: &[String]) -> u32 {
    fields
        .iter()
        .map(|f| f.split('.').filter(|p| !p.trim().is_empty()).count() as u32)
        .max()
        .unwrap_or(1)
        .max(1)
}

fn filter_condition_depth(v: &serde_json::Value) -> u32 {
    match v {
        serde_json::Value::Object(map) => {
            1 + map.values().map(filter_condition_depth).max().unwrap_or(0)
        }
        serde_json::Value::Array(items) => {
            items.iter().map(filter_condition_depth).max().unwrap_or(0)
        }
        _ => 0,
    }
}

fn has_operator_keys(value: &serde_json::Value) -> bool {
    let Some(map) = value.as_object() else {
        return false;
    };
    map.keys().any(|k| {
        matches!(
            normalize_token(k).as_str(),
            "ge" | "gte" | "gt" | "le" | "lte" | "lt" | "eq" | "between" | "from" | "to"
        )
    })
}

fn has_time_window_filter_for_fields(
    filter: &serde_json::Value,
    time_fields_normalized: &std::collections::HashSet<String>,
) -> bool {
    match filter {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                let key = normalize_token(k);
                if key == "and" || key == "or" || key == "not" {
                    if has_time_window_filter_for_fields(v, time_fields_normalized) {
                        return true;
                    }
                    continue;
                }
                if time_fields_normalized.contains(&key) {
                    if v.is_string() || v.is_number() || has_operator_keys(v) {
                        return true;
                    }
                    if has_time_window_filter_for_fields(v, time_fields_normalized) {
                        return true;
                    }
                } else if has_time_window_filter_for_fields(v, time_fields_normalized) {
                    return true;
                }
            }
            false
        }
        serde_json::Value::Array(items) => items
            .iter()
            .any(|x| has_time_window_filter_for_fields(x, time_fields_normalized)),
        _ => false,
    }
}

fn step_id_for(plan: &PlanV2, idx: usize) -> String {
    plan.steps
        .get(idx)
        .map(|s| s.id.clone())
        .unwrap_or_else(|| format!("s{}", idx + 1))
}

fn requires_time_window(root_field: &str, sls: &Sls) -> bool {
    sls.derived
        .required_time_window_roots
        .contains(&root_field.to_lowercase())
}

fn source_refs_for_step(op: &PlanV2Op) -> Vec<&str> {
    match op {
        PlanV2Op::Aggregate { source, .. }
        | PlanV2Op::FilterRows { source, .. }
        | PlanV2Op::Rank { source, .. }
        | PlanV2Op::ThresholdCheck { source, .. }
        | PlanV2Op::TrendSummary { source, .. } => vec![source.as_str()],
        PlanV2Op::Compare { left, right, .. } | PlanV2Op::JoinOnTime { left, right, .. } => {
            vec![left.as_str(), right.as_str()]
        }
        PlanV2Op::DistanceHaversine {
            vessels_source,
            target_source,
            ..
        } => vec![vessels_source.as_str(), target_source.as_str()],
        PlanV2Op::Fetch { .. } => Vec::new(),
    }
}

fn downstream_ops_for_step<'a>(plan: &'a PlanV2, source_id: &str) -> Vec<&'a PlanV2Op> {
    plan.steps
        .iter()
        .filter_map(|step| {
            source_refs_for_step(&step.op)
                .contains(&source_id)
                .then_some(&step.op)
        })
        .collect()
}

fn has_time_window_filter(
    schema_registry: &SchemaRegistry,
    root_field: &str,
    filter: &serde_json::Value,
) -> bool {
    let time_fields = schema_registry.root_time_filter_fields(root_field);
    if time_fields.is_empty() {
        return false;
    }
    let normalized = time_fields
        .iter()
        .map(|f| normalize_token(f))
        .filter(|f| !f.is_empty())
        .collect::<std::collections::HashSet<_>>();
    has_time_window_filter_for_fields(filter, &normalized)
}

fn effective_fetch_row_limit(
    plan: &PlanV2,
    step_idx: usize,
    root_field: &str,
    filter: Option<&serde_json::Value>,
    base_max_rows: i64,
    schema_registry: &SchemaRegistry,
) -> i64 {
    let step_id = step_id_for(plan, step_idx);
    let downstream = downstream_ops_for_step(plan, &step_id);
    if downstream.iter().any(|op| {
        matches!(
            op,
            PlanV2Op::Aggregate { .. }
                | PlanV2Op::Rank { .. }
                | PlanV2Op::Compare { .. }
                | PlanV2Op::TrendSummary { .. }
        )
    }) {
        return base_max_rows;
    }
    if filter.is_some_and(|f| has_time_window_filter(schema_registry, root_field, f)) {
        return base_max_rows;
    }
    if downstream.iter().any(|op| {
        matches!(
            op,
            PlanV2Op::FilterRows { .. }
                | PlanV2Op::JoinOnTime { .. }
                | PlanV2Op::DistanceHaversine { .. }
                | PlanV2Op::ThresholdCheck { .. }
        )
    }) {
        return base_max_rows.min(1000);
    }
    if filter.is_some() {
        return base_max_rows.min(500);
    }
    base_max_rows.min(200)
}

pub(crate) fn policy_hints_for_prompt(sls: Option<&Sls>) -> String {
    let Some(sls) = sls else {
        return String::new();
    };
    let Some(policies) = sls.policies.as_ref() else {
        return String::new();
    };
    let Some(limits) = policies.limits.as_ref() else {
        return String::new();
    };

    let mut roots = sls
        .derived
        .required_time_window_roots
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    roots.sort();

    let mut payload = serde_json::Map::new();
    payload.insert(
        "max_rows_per_fetch".to_string(),
        serde_json::json!(limits.max_rows),
    );
    payload.insert(
        "max_selection_depth".to_string(),
        serde_json::json!(limits.max_depth),
    );
    payload.insert(
        "max_filter_condition_depth".to_string(),
        serde_json::json!(limits.max_depth),
    );
    payload.insert(
        "max_complexity".to_string(),
        serde_json::json!(limits.max_complexity),
    );
    if !policies.field_allowlists.is_empty() {
        payload.insert(
            "field_allowlists".to_string(),
            serde_json::json!(policies.field_allowlists),
        );
    }
    if let Some(aggregation) = policies.aggregation.as_ref() {
        payload.insert(
            "aggregation".to_string(),
            serde_json::json!({
                "max_group_by_fields": aggregation.max_group_by_fields,
                "max_groups": aggregation.max_groups,
                "require_time_window_for_metrics": aggregation.require_time_window_for_metrics,
            }),
        );
    }
    payload.insert(
        "required_time_window_roots".to_string(),
        serde_json::json!(roots),
    );

    format!(
        "SLS policy constraints (JSON, must be respected):\n{}\n",
        serde_json::to_string_pretty(&serde_json::Value::Object(payload))
            .unwrap_or_else(|_| "{}".to_string())
    )
}

pub(crate) fn evaluate_plan_policies(
    plan: &PlanV2,
    sls: Option<&Sls>,
    schema_registry: &SchemaRegistry,
) -> PolicyEvaluation {
    let Some(sls) = sls else {
        return PolicyEvaluation::default();
    };
    let Some(limits) = sls.policies.as_ref().and_then(|p| p.limits.as_ref()) else {
        return PolicyEvaluation::default();
    };

    let max_rows = limits.max_rows.map(i64::from);
    let max_depth = limits.max_depth;
    let mut out = PolicyEvaluation::default();

    for (idx, step) in plan.steps.iter().enumerate() {
        if let PlanV2Op::Fetch {
            root_field,
            fields,
            first,
            filter,
            order,
            ..
        } = &step.op
        {
            if let Some(max_rows) = max_rows {
                let effective_max_rows = effective_fetch_row_limit(
                    plan,
                    idx,
                    root_field,
                    filter.as_ref(),
                    max_rows,
                    schema_registry,
                );
                match first {
                    Some(v) if *v > effective_max_rows => out.fixes.push(PolicyFix::CapFirst {
                        step_index: idx,
                        value: effective_max_rows,
                    }),
                    None => out.fixes.push(PolicyFix::SetFirst {
                        step_index: idx,
                        value: effective_max_rows,
                    }),
                    _ => {}
                }
            }

            if let Some(max_depth) = max_depth {
                let sel_depth = selection_depth(fields);
                if sel_depth > max_depth {
                    out.violations.push(format!(
                        "policy violation: step `{}` selection_depth={} exceeds max_selection_depth={}",
                        step.id, sel_depth, max_depth
                    ));
                }

                let filter_depth = filter.as_ref().map_or(0, filter_condition_depth);
                if filter_depth > max_depth {
                    out.violations.push(format!(
                        "policy violation: step `{}` filter_condition_depth={} exceeds max_filter_condition_depth={}",
                        step.id, filter_depth, max_depth
                    ));
                }

                let order_depth = order.as_ref().map_or(0, filter_condition_depth);
                if order_depth > max_depth {
                    out.violations.push(format!(
                        "policy violation: step `{}` order_depth={} exceeds max_filter_condition_depth={}",
                        step.id, order_depth, max_depth
                    ));
                }
            }

            if requires_time_window(root_field, sls) {
                let schema_time_fields = schema_registry.root_time_filter_fields(root_field);
                let normalized = schema_time_fields
                    .iter()
                    .map(|f| normalize_token(f))
                    .filter(|f| !f.is_empty())
                    .collect::<std::collections::HashSet<_>>();

                if normalized.is_empty() {
                    out.violations.push(format!(
                        "policy violation: step `{}` on `{}` requires time window, but no schema time-filter fields were discovered for this root",
                        step.id, root_field
                    ));
                } else if !filter
                    .as_ref()
                    .is_some_and(|f| has_time_window_filter_for_fields(f, &normalized))
                {
                    let mut tf = schema_time_fields.clone();
                    tf.sort();
                    out.violations.push(format!(
                        "policy violation: step `{}` on `{}` requires explicit time window in filter. Allowed time fields: {}",
                        step.id,
                        root_field,
                        tf.join(", ")
                    ));
                }
            }
        }
    }

    let fetch_root_by_step = plan
        .steps
        .iter()
        .filter_map(|s| {
            if let PlanV2Op::Fetch { root_field, .. } = &s.op {
                Some((s.id.clone(), root_field.clone()))
            } else {
                None
            }
        })
        .collect::<std::collections::HashMap<_, _>>();

    for (idx, step) in plan.steps.iter().enumerate() {
        if let PlanV2Op::JoinOnTime {
            left,
            right,
            left_time_field,
            right_time_field,
            window_minutes,
        } = &step.op
        {
            let left_root = fetch_root_by_step.get(left);
            let right_root = fetch_root_by_step.get(right);
            let (Some(left_root), Some(right_root)) = (left_root, right_root) else {
                continue;
            };

            if !sls.preferred_join_paths.is_empty()
                && !sls.is_preferred_join_pair(left_root, right_root)
            {
                let mut allowed = sls
                    .preferred_join_paths
                    .iter()
                    .map(|p| format!("{}<->{}", p.from_root, p.to_root))
                    .collect::<Vec<_>>();
                allowed.sort();
                out.violations.push(format!(
                    "policy violation: step `{}` join pair `{} <-> {}` is not in preferred_join_paths. Allowed pairs: {}",
                    step.id,
                    left_root,
                    right_root,
                    allowed.join(", ")
                ));
                continue;
            }

            if let Some(join_pref) = sls.preferred_join_for_pair(left_root, right_root) {
                if left_time_field.is_none()
                    && let Some(f) = &join_pref.left_time_field
                {
                    out.fixes.push(PolicyFix::SetJoinLeftTimeField {
                        step_index: idx,
                        field: f.clone(),
                    });
                }
                if right_time_field.is_none()
                    && let Some(f) = &join_pref.right_time_field
                {
                    out.fixes.push(PolicyFix::SetJoinRightTimeField {
                        step_index: idx,
                        field: f.clone(),
                    });
                }
                match (window_minutes, join_pref.max_window_minutes) {
                    (None, Some(max_m)) => out.fixes.push(PolicyFix::SetJoinWindowMinutes {
                        step_index: idx,
                        value: max_m,
                    }),
                    (Some(w), Some(max_m)) if *w > max_m => out.violations.push(format!(
                        "policy violation: step `{}` window_minutes={} exceeds preferred_join_paths max_window_minutes={}",
                        step.id, w, max_m
                    )),
                    _ => {}
                }
            }
        }
    }

    out
}

pub(crate) fn apply_policy_fixes(plan: &mut PlanV2, fixes: &[PolicyFix]) -> Vec<String> {
    let mut notes = Vec::new();
    for fix in fixes {
        match fix {
            PolicyFix::SetFirst { step_index, value } => {
                let sid = step_id_for(plan, *step_index);
                if let Some(step) = plan.steps.get_mut(*step_index)
                    && let PlanV2Op::Fetch { first, .. } = &mut step.op
                    && first.is_none()
                {
                    *first = Some(*value);
                    notes.push(format!(
                        "policy_applied: step `{sid}` injected first={} from max_rows policy",
                        value
                    ));
                }
            }
            PolicyFix::CapFirst { step_index, value } => {
                let sid = step_id_for(plan, *step_index);
                if let Some(step) = plan.steps.get_mut(*step_index)
                    && let PlanV2Op::Fetch { first, .. } = &mut step.op
                    && let Some(v) = *first
                    && v > *value
                {
                    *first = Some(*value);
                    notes.push(format!(
                        "policy_applied: step `{sid}` capped first to {} from max_rows policy",
                        value
                    ));
                }
            }
            PolicyFix::SetJoinLeftTimeField { step_index, field } => {
                let sid = step_id_for(plan, *step_index);
                if let Some(step) = plan.steps.get_mut(*step_index)
                    && let PlanV2Op::JoinOnTime {
                        left_time_field, ..
                    } = &mut step.op
                    && left_time_field.is_none()
                {
                    *left_time_field = Some(field.clone());
                    notes.push(format!(
                        "policy_applied: step `{sid}` set left_time_field=`{field}` from preferred_join_paths"
                    ));
                }
            }
            PolicyFix::SetJoinRightTimeField { step_index, field } => {
                let sid = step_id_for(plan, *step_index);
                if let Some(step) = plan.steps.get_mut(*step_index)
                    && let PlanV2Op::JoinOnTime {
                        right_time_field, ..
                    } = &mut step.op
                    && right_time_field.is_none()
                {
                    *right_time_field = Some(field.clone());
                    notes.push(format!(
                        "policy_applied: step `{sid}` set right_time_field=`{field}` from preferred_join_paths"
                    ));
                }
            }
            PolicyFix::SetJoinWindowMinutes { step_index, value } => {
                let sid = step_id_for(plan, *step_index);
                if let Some(step) = plan.steps.get_mut(*step_index)
                    && let PlanV2Op::JoinOnTime { window_minutes, .. } = &mut step.op
                    && window_minutes.is_none()
                {
                    *window_minutes = Some(*value);
                    notes.push(format!(
                        "policy_applied: step `{sid}` set window_minutes={} from preferred_join_paths",
                        value
                    ));
                }
            }
        }
    }
    notes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner::{PlanV2Op, PlanV2Step};
    use crate::schema_registry::SchemaRegistry;
    use crate::sls::{Sls, load_sls_merged};

    const TEST_SCHEMA: &str = include_str!("../schemas/consumer_schema.graphql");

    fn test_sls(max_rows: u32, max_depth: u32, required: &[&str]) -> Sls {
        let bootstrap = SchemaRegistry::new(TEST_SCHEMA);
        let mut sls = load_sls_merged(&bootstrap, "sls.yaml").expect("load sls");
        if let Some(p) = sls.policies.as_mut()
            && let Some(l) = p.limits.as_mut()
        {
            l.max_rows = Some(max_rows);
            l.max_depth = Some(max_depth);
            l.require_time_window_for = Some(required.iter().map(|s| s.to_string()).collect());
        }
        sls
    }

    #[test]
    fn policy_guard_uses_smaller_caps_for_broad_terminal_fetches() {
        let mut plan = PlanV2 {
            version: Some("v2".to_string()),
            rewrites: vec![],
            notes: vec![],
            steps: vec![
                PlanV2Step {
                    id: "s1".to_string(),
                    op: PlanV2Op::Fetch {
                        root_field: "queryVessel".to_string(),
                        fields: vec!["name".to_string()],
                        first: None,
                        offset: None,
                        filter: None,
                        order: None,
                    },
                },
                PlanV2Step {
                    id: "s2".to_string(),
                    op: PlanV2Op::Fetch {
                        root_field: "queryVessel".to_string(),
                        fields: vec!["name".to_string()],
                        first: Some(10_000),
                        offset: None,
                        filter: None,
                        order: None,
                    },
                },
            ],
        };
        let sls = test_sls(1000, 8, &[]);
        let registry = SchemaRegistry::new(TEST_SCHEMA);
        let eval = evaluate_plan_policies(&plan, Some(&sls), &registry);
        assert!(
            eval.violations.is_empty(),
            "unexpected violations: {:?}",
            eval.violations
        );
        let notes = apply_policy_fixes(&mut plan, &eval.fixes);

        let first_s1 = match &plan.steps[0].op {
            PlanV2Op::Fetch { first, .. } => *first,
            _ => None,
        };
        let first_s2 = match &plan.steps[1].op {
            PlanV2Op::Fetch { first, .. } => *first,
            _ => None,
        };
        assert_eq!(first_s1, Some(200));
        assert_eq!(first_s2, Some(200));
        assert!(
            notes.iter().any(|n| n.contains("injected first=200")),
            "expected injected-first note, got {notes:?}"
        );
        assert!(
            notes.iter().any(|n| n.contains("capped first")),
            "expected capped-first note, got {notes:?}"
        );
    }

    #[test]
    fn policy_guard_keeps_high_cap_for_analytical_fetches() {
        let mut plan = PlanV2 {
            version: Some("v2".to_string()),
            rewrites: vec![],
            notes: vec![],
            steps: vec![
                PlanV2Step {
                    id: "s1".to_string(),
                    op: PlanV2Op::Fetch {
                        root_field: "queryOffshoreWindTurbine".to_string(),
                        fields: vec!["accumulatedDowntime".to_string()],
                        first: None,
                        offset: None,
                        filter: None,
                        order: None,
                    },
                },
                PlanV2Step {
                    id: "s2".to_string(),
                    op: PlanV2Op::Aggregate {
                        source: "s1".to_string(),
                        group_by: vec![],
                        metrics: vec![crate::planner::MetricSpec::Avg {
                            field: "accumulatedDowntime".to_string(),
                        }],
                    },
                },
            ],
        };
        let sls = test_sls(1000, 8, &[]);
        let registry = SchemaRegistry::new(TEST_SCHEMA);
        let eval = evaluate_plan_policies(&plan, Some(&sls), &registry);
        assert!(
            eval.violations.is_empty(),
            "unexpected violations: {:?}",
            eval.violations
        );
        let notes = apply_policy_fixes(&mut plan, &eval.fixes);
        let first_s1 = match &plan.steps[0].op {
            PlanV2Op::Fetch { first, .. } => *first,
            _ => None,
        };
        assert_eq!(first_s1, Some(1000));
        assert!(
            notes.iter().any(|n| n.contains("injected first=1000")),
            "expected high analytical cap note, got {notes:?}"
        );
    }

    #[test]
    fn policy_guard_requires_time_filter_for_alarm_roots() {
        let plan = PlanV2 {
            version: Some("v2".to_string()),
            rewrites: vec![],
            notes: vec![],
            steps: vec![PlanV2Step {
                id: "s1".to_string(),
                op: PlanV2Op::Fetch {
                    root_field: "queryScadaEventSignal".to_string(),
                    fields: vec!["tagId".to_string()],
                    first: Some(10),
                    offset: None,
                    filter: Some(serde_json::json!({"tagId": {"eq": "TAG-1"}})),
                    order: None,
                },
            }],
        };
        let sls = test_sls(1000, 8, &["alarms"]);
        let registry = SchemaRegistry::new(TEST_SCHEMA);
        let eval = evaluate_plan_policies(&plan, Some(&sls), &registry);
        assert!(
            eval.violations
                .iter()
                .any(|v| v.contains("requires time window")),
            "expected time-window violation, got {:?}",
            eval.violations
        );
    }

    #[test]
    fn policy_guard_rejects_selection_depth_overflow() {
        let plan = PlanV2 {
            version: Some("v2".to_string()),
            rewrites: vec![],
            notes: vec![],
            steps: vec![PlanV2Step {
                id: "s1".to_string(),
                op: PlanV2Op::Fetch {
                    root_field: "queryVessel".to_string(),
                    fields: vec!["a.b.c.d.e".to_string()],
                    first: Some(10),
                    offset: None,
                    filter: None,
                    order: None,
                },
            }],
        };
        let sls = test_sls(1000, 4, &[]);
        let registry = SchemaRegistry::new(TEST_SCHEMA);
        let eval = evaluate_plan_policies(&plan, Some(&sls), &registry);
        assert!(
            eval.violations
                .iter()
                .any(|v| v.contains("selection_depth")),
            "expected selection-depth violation, got {:?}",
            eval.violations
        );
    }

    #[test]
    fn policy_guard_rejects_non_preferred_join_pair() {
        let plan = PlanV2 {
            version: Some("v2".to_string()),
            rewrites: vec![],
            notes: vec![],
            steps: vec![
                PlanV2Step {
                    id: "s1".to_string(),
                    op: PlanV2Op::Fetch {
                        root_field: "queryVessel".to_string(),
                        fields: vec!["name".to_string()],
                        first: Some(10),
                        offset: None,
                        filter: None,
                        order: None,
                    },
                },
                PlanV2Step {
                    id: "s2".to_string(),
                    op: PlanV2Op::Fetch {
                        root_field: "queryPowerPrediction".to_string(),
                        fields: vec!["time".to_string(), "powerPrediction".to_string()],
                        first: Some(10),
                        offset: None,
                        filter: None,
                        order: None,
                    },
                },
                PlanV2Step {
                    id: "s3".to_string(),
                    op: PlanV2Op::JoinOnTime {
                        left: "s1".to_string(),
                        right: "s2".to_string(),
                        left_time_field: None,
                        right_time_field: None,
                        window_minutes: Some(10),
                    },
                },
            ],
        };
        let sls = test_sls(1000, 8, &[]);
        let registry = SchemaRegistry::new(TEST_SCHEMA);
        let eval = evaluate_plan_policies(&plan, Some(&sls), &registry);
        assert!(
            eval.violations
                .iter()
                .any(|v| v.contains("not in preferred_join_paths")),
            "expected preferred-join violation, got {:?}",
            eval.violations
        );
    }

    #[test]
    fn policy_guard_applies_preferred_join_defaults() {
        let mut plan = PlanV2 {
            version: Some("v2".to_string()),
            rewrites: vec![],
            notes: vec![],
            steps: vec![
                PlanV2Step {
                    id: "s1".to_string(),
                    op: PlanV2Op::Fetch {
                        root_field: "queryPowerPrediction".to_string(),
                        fields: vec!["time".to_string(), "powerPrediction".to_string()],
                        first: Some(10),
                        offset: None,
                        filter: None,
                        order: None,
                    },
                },
                PlanV2Step {
                    id: "s2".to_string(),
                    op: PlanV2Op::Fetch {
                        root_field: "queryHistoricalScadaAgg10min".to_string(),
                        fields: vec!["timestamp".to_string(), "value".to_string()],
                        first: Some(10),
                        offset: None,
                        filter: Some(serde_json::json!({
                            "timestamp": {
                                "ge": "2026-02-16T00:00:00Z",
                                "le": "2026-02-16T01:00:00Z"
                            }
                        })),
                        order: None,
                    },
                },
                PlanV2Step {
                    id: "s3".to_string(),
                    op: PlanV2Op::JoinOnTime {
                        left: "s1".to_string(),
                        right: "s2".to_string(),
                        left_time_field: None,
                        right_time_field: None,
                        window_minutes: None,
                    },
                },
            ],
        };
        let sls = test_sls(1000, 8, &[]);
        let registry = SchemaRegistry::new(TEST_SCHEMA);
        let eval = evaluate_plan_policies(&plan, Some(&sls), &registry);
        assert!(
            eval.violations.is_empty(),
            "unexpected violations: {:?}",
            eval.violations
        );
        let notes = apply_policy_fixes(&mut plan, &eval.fixes);
        let (left_tf, right_tf, win) = match &plan.steps[2].op {
            PlanV2Op::JoinOnTime {
                left_time_field,
                right_time_field,
                window_minutes,
                ..
            } => (
                left_time_field.clone(),
                right_time_field.clone(),
                *window_minutes,
            ),
            _ => (None, None, None),
        };
        assert_eq!(left_tf.as_deref(), Some("time"));
        assert_eq!(right_tf.as_deref(), Some("timestamp"));
        assert_eq!(win, Some(10));
        assert!(
            notes.iter().any(|n| n.contains("preferred_join_paths")),
            "expected policy_applied join notes, got {notes:?}"
        );
    }
}
