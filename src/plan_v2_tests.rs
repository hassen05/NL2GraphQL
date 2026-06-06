#![allow(clippy::needless_raw_string_hashes)]

use crate::planner::{
    ExecutedArtifact, MetricSpec, PlanV2, PlanV2Op, PlanV2Step, apply_parent_relation_rewrite,
    executed_query_text, parse_plan_v2_struct_from_response, plan_v2_to_multistep,
    resolve_sls_metric_refs, scope_used_summary, synthesize_simple_fetch_plan, validate_plan_v2,
    validate_sls_metric_sources,
};
use crate::query_repair::maybe_build_empty_rows_retry;
use crate::query_repair::maybe_build_error_retry;
use crate::query_repair::maybe_expand_identifier_eq_filter;
use crate::schema_registry::SchemaRegistry;

fn registry() -> SchemaRegistry {
    SchemaRegistry::new(include_str!("../schemas/consumer_schema.graphql"))
}

fn merged_sls() -> crate::sls::Sls {
    crate::sls::load_sls_merged(&registry(), "sls.yaml").expect("load merged sls")
}

fn registry_with_sls(sls: &crate::sls::Sls) -> SchemaRegistry {
    SchemaRegistry::with_sls(
        include_str!("../schemas/consumer_schema.graphql"),
        Some(sls),
    )
}

#[test]
fn plan_v2_valid_fetch_passes_strict_validation() {
    let response = r#"{
        "version": "v2",
        "rewrites": ["plan_v2"],
        "notes": [],
        "steps": [
            {
                "id": "s1",
                "op": "fetch",
                "root_field": "queryVessel",
                "fields": ["name", "mmsi"],
                "first": 1,
                "offset": 0,
                "filter": {"name": {"eq": "the wagon"}},
                "order": null
            }
        ]
    }"#;
    let registry = registry();
    let plan = parse_plan_v2_struct_from_response(response).expect("should parse");
    assert!(validate_plan_v2(&plan, &registry).is_ok());
    let compiled = plan_v2_to_multistep(&plan).expect("should compile");
    assert!(
        compiled
            .steps
            .iter()
            .any(|s| matches!(s.op, crate::planner::PlanV2Op::Fetch { .. }) && s.query.is_some()),
        "expected compiled fetch step to retain GraphQL query, got {:?}",
        compiled.steps
    );
}

#[test]
fn plan_v2_invalid_root_is_rejected() {
    let response = r#"{
        "version": "v2",
        "rewrites": [],
        "notes": [],
        "steps": [
            {
                "id": "s1",
                "op": "fetch",
                "root_field": "queryUnknown",
                "fields": ["name"],
                "first": null,
                "offset": null,
                "filter": null,
                "order": null
            }
        ]
    }"#;
    let registry = registry();
    let plan = parse_plan_v2_struct_from_response(response).expect("should parse");
    let err = validate_plan_v2(&plan, &registry).expect_err("must reject invalid root");
    assert!(err.to_string().contains("root_field"));
    assert!(err.to_string().contains("queryUnknown"));
}

#[test]
fn parser_normalizes_filter_rows_input_filter_shape() {
    let response = r#"{
        "version": "v2",
        "rewrites": [],
        "notes": ["Filter turbines where accumulatedDowntime is greater than 400."],
        "steps": [
            {
                "id": "s1",
                "op": "fetch",
                "root_field": "queryOffshoreWindTurbine",
                "fields": ["shortName", "name", "accumulatedDowntime"],
                "filter": null,
                "order": null,
                "first": 2000,
                "offset": null
            },
            {
                "id": "s2",
                "op": "filter_rows",
                "input": "s1",
                "filter": {
                    "accumulatedDowntime": {
                        "gt": 400
                    }
                }
            }
        ]
    }"#;

    let plan = parse_plan_v2_struct_from_response(response).expect("should parse");
    assert_eq!(plan.steps.len(), 2);
    match &plan.steps[1].op {
        PlanV2Op::FilterRows {
            source,
            field,
            operator,
            value,
        } => {
            assert_eq!(source, "s1");
            assert_eq!(field, "accumulatedDowntime");
            assert_eq!(operator, "gt");
            assert_eq!(value, &serde_json::json!(400));
        }
        other => panic!("expected normalized filter_rows step, got {other:?}"),
    }
}

#[test]
fn parser_normalizes_filter_rows_input_ref_object_shape() {
    let response = r#"{
        "version": "v2",
        "rewrites": [],
        "notes": ["Filter turbines where accumulatedDowntime is greater than 400."],
        "steps": [
            {
                "id": "s1",
                "op": "fetch",
                "root_field": "queryOffshoreWindTurbine",
                "fields": ["shortName", "name", "accumulatedDowntime"],
                "filter": null,
                "order": null,
                "first": 2000,
                "offset": null
            },
            {
                "id": "s2",
                "op": "filter_rows",
                "input": {"ref": "s1"},
                "filter": {
                    "accumulatedDowntime": {
                        "gt": 400
                    }
                }
            }
        ]
    }"#;

    let plan = parse_plan_v2_struct_from_response(response).expect("should parse");
    assert_eq!(plan.steps.len(), 2);
    match &plan.steps[1].op {
        PlanV2Op::FilterRows {
            source,
            field,
            operator,
            value,
        } => {
            assert_eq!(source, "s1");
            assert_eq!(field, "accumulatedDowntime");
            assert_eq!(operator, "gt");
            assert_eq!(value, &serde_json::json!(400));
        }
        other => panic!("expected normalized filter_rows step, got {other:?}"),
    }
}

#[test]
fn parser_normalizes_filter_rows_placeholder_input_condition_shape() {
    let response = r#"{
        "version": "v2",
        "rewrites": [],
        "notes": ["Fetch turbines and then filter by accumulatedDowntime > 400."],
        "steps": [
            {
                "id": "s1",
                "op": "fetch",
                "root_field": "queryOffshoreWindTurbine",
                "fields": ["shortName", "name", "accumulatedDowntime"],
                "filter": null,
                "order": null,
                "first": 2000,
                "offset": null
            },
            {
                "id": "s2",
                "op": "filter_rows",
                "input": "${s1}",
                "condition": {
                    "accumulatedDowntime": {
                        "gt": 400
                    }
                }
            }
        ]
    }"#;

    let plan = parse_plan_v2_struct_from_response(response).expect("should parse");
    assert_eq!(plan.steps.len(), 2);
    match &plan.steps[1].op {
        PlanV2Op::FilterRows {
            source,
            field,
            operator,
            value,
        } => {
            assert_eq!(source, "s1");
            assert_eq!(field, "accumulatedDowntime");
            assert_eq!(operator, "gt");
            assert_eq!(value, &serde_json::json!(400));
        }
        other => panic!("expected normalized filter_rows step, got {other:?}"),
    }
}

#[test]
fn parser_drops_noop_output_step_after_filter_rows_repair() {
    let response = r#"{
        "version": "v2",
        "rewrites": [],
        "notes": ["Filter turbines by accumulatedDowntime > 400."],
        "steps": [
            {
                "id": "s1",
                "op": "fetch",
                "root_field": "queryOffshoreWindTurbine",
                "fields": ["name", "shortName", "accumulatedDowntime"],
                "filter": null,
                "order": null,
                "first": 2000,
                "offset": null
            },
            {
                "id": "s2",
                "op": "filter_rows",
                "source": "s1",
                "filter": {
                    "accumulatedDowntime": {
                        "gt": 400
                    }
                }
            },
            {
                "id": "s3",
                "op": "output",
                "source": "s2"
            }
        ]
    }"#;

    let plan = parse_plan_v2_struct_from_response(response).expect("should parse");
    assert_eq!(
        plan.steps.len(),
        2,
        "expected no-op output step to be dropped"
    );
    match &plan.steps[1].op {
        PlanV2Op::FilterRows {
            source,
            field,
            operator,
            value,
        } => {
            assert_eq!(source, "s1");
            assert_eq!(field, "accumulatedDowntime");
            assert_eq!(operator, "gt");
            assert_eq!(value, &serde_json::json!(400));
        }
        other => panic!("expected normalized filter_rows step, got {other:?}"),
    }
}

#[test]
fn parent_relation_rewrite_uses_parent_fetch_for_child_filter() {
    let sls = merged_sls();
    let registry = registry_with_sls(&sls);
    let mut plan = PlanV2 {
        version: Some("v2".to_string()),
        rewrites: vec!["plan_v2".to_string()],
        notes: vec![],
        steps: vec![PlanV2Step {
            id: "s1".to_string(),
            op: PlanV2Op::Fetch {
                root_field: "queryOffshoreWindTurbine".to_string(),
                fields: vec![
                    "name".to_string(),
                    "partOfOffshoreWindFarmUid".to_string(),
                    "shortName".to_string(),
                ],
                first: None,
                offset: None,
                filter: Some(serde_json::json!({
                    "partOfOffshoreWindFarmUid": { "contains": "Wind Farm 3" }
                })),
                order: None,
            },
        }],
    };

    let changed = apply_parent_relation_rewrite(
        &mut plan,
        "List turbines in wind farm Wind Farm 3",
        &registry,
        Some(&sls),
    );
    assert!(changed, "expected rewrite to apply");
    let fetch_step = plan
        .steps
        .iter()
        .find_map(|step| {
            if let PlanV2Op::Fetch {
                root_field,
                fields,
                filter,
                ..
            } = &step.op
            {
                Some((root_field, fields, filter))
            } else {
                None
            }
        })
        .expect("expected fetch step");
    assert_eq!(fetch_step.0, "queryOffshoreWindFarm");
    assert!(
        fetch_step
            .1
            .iter()
            .any(|f| f.starts_with("hasOffshoreWindTurbine.")),
        "expected nested turbine fields in fetch"
    );
    assert!(fetch_step.2.is_some(), "expected parent filter");
}

#[test]
fn parent_relation_rewrite_collapses_placeholder_child_fetch() {
    let sls = merged_sls();
    let registry = registry_with_sls(&sls);
    let mut plan = PlanV2 {
        version: Some("v2".to_string()),
        rewrites: vec!["plan_v2".to_string()],
        notes: vec![],
        steps: vec![
            PlanV2Step {
                id: "s1".to_string(),
                op: PlanV2Op::Fetch {
                    root_field: "queryOffshoreWindFarm".to_string(),
                    fields: vec!["plantId".to_string()],
                    first: Some(1),
                    offset: None,
                    filter: Some(serde_json::json!({
                        "name": { "eq": "Wind Farm 3" }
                    })),
                    order: None,
                },
            },
            PlanV2Step {
                id: "s2".to_string(),
                op: PlanV2Op::Fetch {
                    root_field: "queryOffshoreSubstation".to_string(),
                    fields: vec![
                        "name".to_string(),
                        "shortName".to_string(),
                        "sapLocationId".to_string(),
                    ],
                    first: Some(2000),
                    offset: None,
                    filter: Some(serde_json::json!({
                        "partOfOffshoreWindFarmUid": { "eq": "${s1.plantId}" }
                    })),
                    order: None,
                },
            },
        ],
    };

    let changed = apply_parent_relation_rewrite(
        &mut plan,
        r#"List offshore substations for wind farm "Wind Farm 3"."#,
        &registry,
        Some(&sls),
    );
    assert!(changed, "expected placeholder child fetch rewrite");
    assert_eq!(plan.steps.len(), 1);
    let fetch_step = plan
        .steps
        .iter()
        .find_map(|step| {
            if let PlanV2Op::Fetch {
                root_field,
                fields,
                filter,
                ..
            } = &step.op
            {
                Some((root_field, fields, filter))
            } else {
                None
            }
        })
        .expect("expected fetch step");
    assert_eq!(fetch_step.0, "queryOffshoreWindFarm");
    assert!(
        fetch_step
            .1
            .iter()
            .any(|f| f == "hasOffshoreSubstation.name"),
        "expected nested offshore substation fields in fetch: {:?}",
        fetch_step.1
    );
    assert!(fetch_step.2.is_some(), "expected parent filter");
}

#[test]
fn plan_v2_invalid_filter_key_is_rejected_before_execution() {
    let response = r#"{
        "version": "v2",
        "rewrites": [],
        "notes": [],
        "steps": [
            {
                "id": "s1",
                "op": "fetch",
                "root_field": "queryVessel",
                "fields": ["name"],
                "first": 5,
                "offset": null,
                "filter": {"unknownField": {"eq": "the wagon"}},
                "order": null
            }
        ]
    }"#;
    let registry = registry();
    let plan = parse_plan_v2_struct_from_response(response).expect("should parse");
    let err = validate_plan_v2(&plan, &registry).expect_err("must reject invalid filter key");
    assert!(err.to_string().contains("unknownField"));
    assert!(err.to_string().contains("input type"));
}

#[test]
fn plan_v2_invalid_filter_shape_is_rejected_before_execution() {
    let response = r#"{
        "version": "v2",
        "rewrites": [],
        "notes": [],
        "steps": [
            {
                "id": "s1",
                "op": "fetch",
                "root_field": "queryVessel",
                "fields": ["name"],
                "first": 5,
                "offset": null,
                "filter": {"name": "the wagon"},
                "order": null
            }
        ]
    }"#;
    let registry = registry();
    let plan = parse_plan_v2_struct_from_response(response).expect("should parse");
    let err = validate_plan_v2(&plan, &registry).expect_err("must reject invalid filter shape");
    assert!(
        err.to_string()
            .contains("Expected object for input type 'StringHashFilter'")
    );
}

#[test]
fn plan_v2_filter_or_list_is_accepted() {
    let response = r#"{
        "version": "v2",
        "rewrites": [],
        "notes": [],
        "steps": [
            {
                "id": "s1",
                "op": "fetch",
                "root_field": "queryVessel",
                "fields": ["name", "mmsi"],
                "first": 5,
                "offset": null,
                "filter": {"or": [{"name": {"eq": "Alpha"}}, {"name": {"eq": "Bravo"}}]},
                "order": null
            }
        ]
    }"#;
    let registry = registry();
    let plan = parse_plan_v2_struct_from_response(response).expect("should parse");
    let result = validate_plan_v2(&plan, &registry);
    assert!(
        result.is_ok(),
        "expected OR list to validate, got {}",
        result.unwrap_err()
    );
}

#[test]
fn plan_v2_invalid_filter_operator_is_rejected_before_execution() {
    let response = r#"{
        "version": "v2",
        "rewrites": [],
        "notes": [],
        "steps": [
            {
                "id": "s1",
                "op": "fetch",
                "root_field": "queryVessel",
                "fields": ["name"],
                "first": 5,
                "offset": null,
                "filter": {"name": {"contains": "the wagon"}},
                "order": null
            }
        ]
    }"#;
    let registry = registry();
    let plan = parse_plan_v2_struct_from_response(response).expect("should parse");
    let err = validate_plan_v2(&plan, &registry).expect_err("must reject invalid operator");
    assert!(err.to_string().contains("contains"));
    assert!(err.to_string().contains("StringHashFilter"));
}

#[test]
fn plan_v2_invalid_order_enum_is_rejected_before_execution() {
    let response = r#"{
        "version": "v2",
        "rewrites": [],
        "notes": [],
        "steps": [
            {
                "id": "s1",
                "op": "fetch",
                "root_field": "queryVessel",
                "fields": ["name"],
                "first": 5,
                "offset": null,
                "filter": null,
                "order": {"asc": "unknownSortableField"}
            }
        ]
    }"#;
    let registry = registry();
    let plan = parse_plan_v2_struct_from_response(response).expect("should parse");
    let err = validate_plan_v2(&plan, &registry).expect_err("must reject invalid order enum");
    assert!(err.to_string().contains("unknownSortableField"));
    assert!(err.to_string().contains("VesselOrderable"));
}

#[test]
fn plan_v2_order_requires_single_direction() {
    let response = r#"{
        "version": "v2",
        "rewrites": [],
        "notes": [],
        "steps": [
            {
                "id": "s1",
                "op": "fetch",
                "root_field": "queryVessel",
                "fields": ["name"],
                "first": 5,
                "offset": null,
                "filter": null,
                "order": {"asc": "name", "desc": "mmsi"}
            }
        ]
    }"#;
    let registry = registry();
    let plan = parse_plan_v2_struct_from_response(response).expect("should parse");
    let err = validate_plan_v2(&plan, &registry).expect_err("must reject conflicting order");
    assert!(err.to_string().contains("asc"));
    assert!(err.to_string().contains("desc"));
}

#[test]
fn plan_v2_accepts_typed_metric_specs() {
    let response = r#"{
        "version": "v2",
        "rewrites": [],
        "notes": [],
        "steps": [
            {
                "id": "s1",
                "op": "fetch",
                "root_field": "queryVessel",
                "fields": ["name", "mmsi", "capacityCrew"],
                "first": 5,
                "offset": null,
                "filter": null,
                "order": null
            },
            {
                "id": "s2",
                "op": "aggregate",
                "source": "s1",
                "group_by": ["name"],
                "metrics": ["count", {"op": "avg", "field": "capacityCrew"}]
            },
            {
                "id": "s3",
                "op": "compare",
                "left": "s1",
                "right": "s1",
                "metric": "avg:capacityCrew"
            }
        ]
    }"#;
    let registry = registry();
    let plan = parse_plan_v2_struct_from_response(response).expect("should parse");
    let result = validate_plan_v2(&plan, &registry);
    assert!(
        result.is_ok(),
        "expected plan to validate, got {}",
        result.unwrap_err()
    );
}

#[test]
fn plan_v2_resolves_sls_metric_refs() {
    let response = r#"{
        "version": "v2",
        "rewrites": [],
        "notes": [],
        "steps": [
            {
                "id": "s1",
                "op": "fetch",
                "root_field": "queryWeatherPrediction",
                "fields": ["time", "windSpeed10m"],
                "first": 5,
                "offset": null,
                "filter": null,
                "order": null
            },
            {
                "id": "s2",
                "op": "aggregate",
                "source": "s1",
                "group_by": [],
                "metrics": [{"op": "metric", "name": "wind_speed"}]
            }
        ]
    }"#;
    let registry = registry();
    let sls = merged_sls();
    let mut plan = parse_plan_v2_struct_from_response(response).expect("should parse");
    resolve_sls_metric_refs(&mut plan, Some(&sls)).expect("resolve metric refs");
    validate_sls_metric_sources(&plan, &registry, Some(&sls)).expect("validate metric sources");
    let result = validate_plan_v2(&plan, &registry);
    assert!(
        result.is_ok(),
        "expected plan to validate after resolving metrics, got {}",
        result.unwrap_err()
    );
    let metrics = match &plan.steps[1].op {
        PlanV2Op::Aggregate { metrics, .. } => metrics.clone(),
        _ => panic!("expected aggregate step"),
    };
    assert!(
        matches!(
            metrics.first(),
            Some(MetricSpec::Formula { name, expr })
                if name == "wind_speed" && expr.contains("windSpeed10m")
        ),
        "expected wind_speed to resolve to formula, got {metrics:?}"
    );
}

#[test]
fn sls_metric_source_validation_rejects_mismatch() {
    let response = r#"{
        "version": "v2",
        "rewrites": [],
        "notes": [],
        "steps": [
            {
                "id": "s1",
                "op": "fetch",
                "root_field": "queryOffshoreWindFarm",
                "fields": ["name"],
                "first": 5,
                "offset": null,
                "filter": null,
                "order": null
            },
            {
                "id": "s2",
                "op": "aggregate",
                "source": "s1",
                "group_by": [],
                "metrics": [{"op": "metric", "name": "wind_speed"}]
            }
        ]
    }"#;
    let registry = registry();
    let sls = merged_sls();
    let mut plan = parse_plan_v2_struct_from_response(response).expect("should parse");
    resolve_sls_metric_refs(&mut plan, Some(&sls)).expect("resolve metric refs");
    let err = validate_sls_metric_sources(&plan, &registry, Some(&sls)).expect_err("should fail");
    assert!(
        err.contains("wind_speed"),
        "expected error to mention metric name, got {err}"
    );
}

#[test]
fn compare_requires_metric() {
    let response = r#"{
        "version": "v2",
        "rewrites": [],
        "notes": [],
        "steps": [
            {
                "id": "s1",
                "op": "fetch",
                "root_field": "queryVessel",
                "fields": ["name"],
                "first": 5,
                "offset": null,
                "filter": null,
                "order": null
            },
            {
                "id": "s2",
                "op": "compare",
                "left": "s1",
                "right": "s1",
                "metric": null
            }
        ]
    }"#;
    let registry = registry();
    let plan = parse_plan_v2_struct_from_response(response).expect("should parse");
    let err = validate_plan_v2(&plan, &registry).expect_err("must reject missing metric");
    assert!(err.to_string().contains("compare requires a metric"));
}

#[test]
fn simple_fetch_fallback_builds_turbine_detail_lookup_plan() {
    let registry = registry();
    let plan = synthesize_simple_fetch_plan(&registry, "List the details of turbine 115", None);
    assert!(
        plan.is_none(),
        "plain-name fallback should not guess a turbine detail lookup"
    );
}

#[test]
fn simple_fetch_fallback_keeps_label_fields_for_direct_entity_lookup_without_details_keyword() {
    let registry = registry();
    let plan = synthesize_simple_fetch_plan(&registry, "Show wind farm Wind Farm 1", None);
    assert!(
        plan.is_none(),
        "plain-name direct entity lookup should not be guessed by fallback"
    );
}

#[test]
fn simple_fetch_fallback_builds_contains_plan_for_short_name_queries() {
    let registry = registry();
    let sls = merged_sls();
    let plan = synthesize_simple_fetch_plan(
        &registry,
        "Get offshore wind farms with shortName containing 'A'.",
        Some(&sls),
    )
    .expect("expected fallback plan");
    assert_eq!(plan.version.as_deref(), Some("v2"));
    assert_eq!(plan.steps.len(), 2, "expected fetch plus local filter_rows");
    match &plan.steps[0].op {
        PlanV2Op::Fetch {
            root_field, fields, ..
        } => {
            assert_eq!(root_field, "queryOffshoreWindFarm");
            assert!(
                fields.iter().any(|f| f == "shortName"),
                "expected fetch to include explicit filter field"
            );
        }
        other => panic!("unexpected first step: {other:?}"),
    }
    match &plan.steps[1].op {
        PlanV2Op::FilterRows {
            source,
            field,
            operator,
            value,
        } => {
            assert_eq!(source, "s1");
            assert_eq!(field, "shortName");
            assert_eq!(operator, "contains");
            assert_eq!(value, &serde_json::json!("A"));
        }
        other => panic!("unexpected second step: {other:?}"),
    }
}

#[test]
fn simple_fetch_fallback_uses_list_filter_when_filter_field_is_list() {
    let registry = registry();
    let sls = merged_sls();
    let plan = synthesize_simple_fetch_plan(
        &registry,
        "List function params where id equals 'FP-123'",
        Some(&sls),
    )
    .expect("expected fallback plan");
    assert_eq!(plan.version.as_deref(), Some("v2"));
    assert_eq!(plan.steps.len(), 1, "expected single fetch step");
    match &plan.steps[0].op {
        PlanV2Op::Fetch {
            root_field, filter, ..
        } => {
            assert_eq!(root_field, "queryFunctionParam");
            let filter = filter.as_ref().expect("expected filter");
            let ids = filter
                .get("id")
                .and_then(|v| v.as_array())
                .expect("expected id list filter");
            assert!(ids.iter().any(|v| v.as_str() == Some("FP-123")));
        }
        other => panic!("unexpected fallback op: {other:?}"),
    }
}

#[test]
fn query_repair_prunes_unknown_selected_fields_from_backend_error() {
    let registry = registry();
    let candidate = r#"query AutoIR {
  queryOffshoreWindFarm(first: 2000) {
    commercialDateTimeOfOperation
    locationId
    locationLabel
    name
    plantId
    shortName
  }
}"#;
    let err = r#"GraphQL execution errors: Unknown field "locationId" on type "OffshoreWindFarm".; Unknown field "locationLabel" on type "OffshoreWindFarm"."#;
    let (_, rewritten) = maybe_build_error_retry(&registry, candidate, err, 0, 4)
        .expect("expected deterministic retry");
    assert!(
        rewritten.contains("queryOffshoreWindFarm"),
        "expected root to be preserved, got:\n{rewritten}"
    );
    assert!(
        rewritten.contains("shortName") && rewritten.contains("plantId"),
        "expected valid fields to be preserved, got:\n{rewritten}"
    );
    assert!(
        !rewritten.contains("locationId") && !rewritten.contains("locationLabel"),
        "expected invalid selected fields to be pruned, got:\n{rewritten}"
    );
}

#[test]
fn plan_v2_allows_fetch_filter_placeholder_binding() {
    let response = r#"{
        "version": "v2",
        "rewrites": [],
        "notes": [],
        "steps": [
            {
                "id": "s0",
                "op": "fetch",
                "root_field": "queryVessel",
                "fields": ["name", "mmsi"],
                "first": null,
                "offset": null,
                "filter": {"name": {"eq": "the wagon"}},
                "order": null
            },
            {
                "id": "s1",
                "op": "fetch",
                "root_field": "queryHistoricalAisVesselpos",
                "fields": ["mmsi", "lat", "lon", "messageTimestamp"],
                "first": null,
                "offset": null,
                "filter": {"mmsi": {"eq": "${s0.mmsi}"}},
                "order": {"desc": "messageTimestamp"}
            }
        ]
    }"#;
    let registry = registry();
    let plan = parse_plan_v2_struct_from_response(response).expect("should parse");
    validate_plan_v2(&plan, &registry).expect("fetch filter placeholders should validate");
}

#[test]
fn scope_used_matches_placeholder_bound_constraint_after_execution() {
    let planned = r#"query AutoIR {
  queryHistoricalAisVesselpos(
    first: 1,
    filter: {mmsi: {eq: "${s1.mmsi}"}},
    order: {desc: messageTimestamp}
  ) {
    lat
    lon
    mmsi
    messageTimestamp
  }
}"#;
    let executed = r#"query AutoIR {
  queryHistoricalAisVesselpos(
    first: 1,
    filter: {mmsi: {eq: 123456789}},
    order: {desc: messageTimestamp}
  ) {
    lat
    lon
    mmsi
    messageTimestamp
  }
}"#;
    let scope = scope_used_summary(planned, executed);
    let missing = scope
        .get("missing_constraints")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(
        missing.is_empty(),
        "placeholder-bound constraint should match executed value, got {missing:?}"
    );
}

#[test]
fn scope_used_matches_expanded_identifier_in_filter() {
    let planned = r#"
      query AutoIR {
        queryTag(filter: {plantId: {eq: "PLANT-2"}}) { categoryDescription }
      }
    "#;
    let executed = r#"
      query AutoIR {
        queryTag(filter: {plantId: {in: ["PLANT-2", "PLANT- 2", "PLANT-  2", "PLANT-002"]}}) { categoryDescription }
      }
    "#;

    let scope = scope_used_summary(planned, executed);
    let missing = scope
        .get("missing_constraints")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(
        missing.is_empty(),
        "identifier expansion should preserve the planned plantId scope, got {missing:?}"
    );
}

#[test]
fn scope_used_detects_missing_location_constraint() {
    let planned_query = r#"query PredictedPower {
        queryPowerPrediction(filter: {location: {eq: "Dudgeon"}, time: {ge: "2026-02-16", le: "2026-02-22"}}) { time powerPrediction }
    }"#;
    let executed_query = r#"query PredictedPower {
        queryPowerPrediction(filter: {time: {ge: "2026-02-16", le: "2026-02-22"}}) { time powerPrediction }
    }"#;
    let scope = scope_used_summary(planned_query, executed_query);
    let missing_values = scope
        .get("missing_constraints")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|v| v.get("values"))
        .filter_map(|v| v.as_array())
        .flat_map(|arr| arr.iter())
        .filter_map(|v| v.as_str().map(str::to_lowercase))
        .collect::<Vec<_>>();
    assert!(
        missing_values.iter().any(|m| m.contains("dudgeon")),
        "expected missing Dudgeon constraint in {missing_values:?}"
    );
}

#[test]
fn scope_used_marks_distance_constraints_as_matched() {
    let planned_query = r#"query {
        queryVessel(filter: {name: {eq: "the wagon"}}) { name mmsi }
        queryOffshoreWindTurbine(filter: {shortName: {eq: "T3"}}) { shortName }
    }"#;
    let executed_query = r#"query {
        queryVessel(filter: {name: {eq: "the wagon"}}) { name mmsi }
        queryOffshoreWindTurbine(filter: {shortName: {eq: "T3"}}) { shortName }
    }"#;
    let scope = scope_used_summary(planned_query, executed_query);
    let missing = scope
        .get("missing_constraints")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(
        missing.is_empty(),
        "unexpected missing constraints: {missing:?}"
    );
}

#[test]
fn scope_used_does_not_require_literal_details_suffix() {
    let planned_query = r#"query {
        queryOffshoreWindTurbine(filter: {shortName: {eq: "T3"}}) {
            shortName
            name
        }
    }"#;
    let executed_query = r#"query {
        queryOffshoreWindTurbine(filter: {shortName: {eq: "T3"}}) {
            shortName
            name
            locationId
        }
    }"#;
    let scope = scope_used_summary(planned_query, executed_query);
    let missing = scope
        .get("missing_constraints")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(
        missing.is_empty(),
        "unexpected missing constraints for details prompt: {missing:?}"
    );
}

#[test]
fn scope_used_ignores_debug_logs_when_collecting_executed_roots() {
    let planned_query = r#"query {
        queryOffshoreWindFarm(filter: {shortName: {contains: "W"}}) {
            name
            shortName
        }
    }"#;
    let executed_query = r#"query {
        queryOffshoreWindFarm(first: 2000) {
            name
            shortName
        }
    }"#;
    let artifacts = vec![
        ExecutedArtifact::query("Query 1 (s1) (effective)", executed_query),
        ExecutedArtifact::debug_log(
            "DEBUG_PREP_LOGS",
            "[STEP_OUTPUT] s1 (query queryOffshoreWindFarm) -> 6 row(s)",
        ),
    ];
    let scope = scope_used_summary(planned_query, &executed_query_text(&artifacts));
    let executed_roots = scope
        .get("executed_roots")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|value| value.as_str().map(str::to_string))
        .collect::<Vec<_>>();
    let missing_roots = scope
        .get("missing_roots")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(
        executed_roots
            .iter()
            .any(|root| root == "queryOffshoreWindFarm"),
        "expected executed roots to keep the real query root, got {executed_roots:?}"
    );
    assert!(
        missing_roots.is_empty(),
        "unexpected missing roots when debug logs are present: {missing_roots:?}"
    );
}

#[test]
fn plan_v2_join_and_threshold_steps_validate() {
    let response = r#"{
        "version": "v2",
        "rewrites": ["plan_v2"],
        "notes": [],
        "steps": [
            {
                "id": "s1",
                "op": "fetch",
                "root_field": "queryVessel",
                "fields": ["mmsi", "name"],
                "first": 5,
                "offset": null,
                "filter": null,
                "order": null
            },
            {
                "id": "s2",
                "op": "fetch",
                "root_field": "queryOffshoreWindTurbine",
                "fields": ["shortName"],
                "first": 5,
                "offset": null,
                "filter": null,
                "order": null
            },
            {
                "id": "s3",
                "op": "join_on_time",
                "left": "s1",
                "right": "s2",
                "window_minutes": 10
            },
            {
                "id": "s4",
                "op": "threshold_check",
                "source": "s3",
                "field": "distanceKm",
                "operator": "<=",
                "value": 1.0
            }
        ]
    }"#;
    let registry = registry();
    let plan = parse_plan_v2_struct_from_response(response).expect("should parse");
    assert!(validate_plan_v2(&plan, &registry).is_ok());
    let compiled = plan_v2_to_multistep(&plan).expect("should compile");
    assert!(
        compiled
            .steps
            .iter()
            .any(|s| s.description.starts_with("Join on time in `s3`")),
        "expected join step text in {:?}",
        compiled.steps
    );
    assert!(
        compiled
            .steps
            .iter()
            .any(|s| s.description.starts_with("Threshold check in `s4`")),
        "expected threshold step text in {:?}",
        compiled.steps
    );
}

#[test]
fn query_repair_does_not_invent_geolocation_path_for_lat_lon_aliases() {
    let registry = SchemaRegistry::new(include_str!("../schemas/consumer_schema.graphql"));
    let candidate = r#"query {
  queryOffshoreWindTurbine(filter: { name: { eq: "T3" } }, first: 1) {
    name
    location {
      lat
      lon
    }
  }
}"#;
    let err = "Query validation: Field 'lat' does not exist on type 'GeoLocation'.";
    assert!(
        maybe_build_error_retry(&registry, candidate, err, 0, 4).is_none(),
        "repair should not choose location.point.latitude/longitude after the planner selected invalid lat/lon aliases"
    );
}

#[test]
fn query_repair_does_not_invent_point_path_for_lat_lon_aliases() {
    let registry = SchemaRegistry::new(include_str!("../schemas/consumer_schema.graphql"));
    let candidate = r#"query GetTurbineLocation {
  queryOffshoreWindTurbine(filter: { name: { eq: "T3" } }) {
    name
    location {
      point {
        lat
        lon
      }
    }
  }
}"#;
    let err = "Query validation: Field 'lat' does not exist on type 'Point'.";
    assert!(
        maybe_build_error_retry(&registry, candidate, err, 0, 4).is_none(),
        "repair should not choose Point.latitude/longitude after the planner selected invalid lat/lon aliases"
    );
}

#[test]
fn query_repair_removes_invalid_filter_field_clause_without_dropping_root() {
    let registry = SchemaRegistry::new(include_str!("../schemas/consumer_schema.graphql"));
    let candidate = r#"query GetTurbineLocation {
  queryOffshoreWindTurbine(filter: {or: [{locationId: {eq: "T3"}}, {name: {eq: "T3"}}, {shortName: {eq: "T3"}}]}) {
    name
    location {
      point {
        latitude
        longitude
      }
    }
  }
}"#;
    let err = r#"HTTP status 400 Bad Request ... Field "locationId" is not defined by type "OffshoreWindTurbineFilter"."#;
    let repaired = maybe_build_error_retry(&registry, candidate, err, 0, 4)
        .expect("expected deterministic invalid-filter-field cleanup")
        .1;
    assert!(
        repaired.contains("queryOffshoreWindTurbine"),
        "root field should remain after repair, got:\n{repaired}"
    );
    assert!(
        !repaired.contains("locationId: {eq: \"T3\"}"),
        "invalid filter clause should be removed, got:\n{repaired}"
    );
}

#[test]
fn empty_rows_identifier_retry_relaxes_first_one_limit() {
    let registry = SchemaRegistry::new(include_str!("../schemas/consumer_schema.graphql"));
    let candidate = r#"query {
  queryOffshoreWindTurbine(first: 1, filter: {name: {eq: "T3"}}) {
    name
    location { point { latitude longitude } }
  }
}"#;
    let rows: Vec<serde_json::Value> = Vec::new();
    let (_msg, retry) = maybe_build_empty_rows_retry(
        &registry,
        candidate,
        0,
        4,
        "queryOffshoreWindTurbine",
        &rows,
    )
    .expect("expected empty-rows retry");
    let rewritten = retry.expect("expected deterministic retry query");
    assert!(
        rewritten.contains("first: 200"),
        "expected first to be relaxed for identifier retry, got:\n{rewritten}"
    );
}

#[test]
fn identifier_fallback_for_short_name_does_not_cross_to_other_fields() {
    let registry = SchemaRegistry::new(include_str!("../schemas/consumer_schema.graphql"));
    let query = r#"query {
  queryOffshoreWindTurbine(filter: {shortName: {eq: "T115"}}) {
    shortName
    name
  }
}"#;
    assert!(
        maybe_expand_identifier_eq_filter(&registry, Some("queryOffshoreWindTurbine"), query)
            .is_none(),
        "repair should not broaden a shortName filter into other identifier/label fields"
    );
}

#[test]
fn identifier_fallback_for_location_id_with_code_does_not_cross_to_name_family() {
    let registry = SchemaRegistry::new(include_str!("../schemas/consumer_schema.graphql"));
    let query = r#"query {
  queryOffshoreWindTurbine(filter: {locationId: {eq: "T115"}}) {
    shortName
    name
  }
}"#;
    assert!(
        maybe_expand_identifier_eq_filter(&registry, Some("queryOffshoreWindTurbine"), query)
            .is_none(),
        "repair should not broaden an id/location filter into label fields"
    );
}

#[test]
fn identifier_fallback_does_not_rewrite_unknown_identifier_filter_key() {
    let registry = SchemaRegistry::new(include_str!("../schemas/consumer_schema.graphql"));
    let query = r#"query {
  queryOffshoreWindTurbine(filter: {turbineId: {eq: "T115"}}) {
    shortName
    name
  }
}"#;
    assert!(
        maybe_expand_identifier_eq_filter(&registry, Some("queryOffshoreWindTurbine"), query)
            .is_none(),
        "repair should not rewrite unknown identifier fields into guessed schema fields"
    );
}
