use crate::schema_registry::SchemaRegistry;
use crate::sls::{
    CanonicalFieldDefaults, Concept, FieldRoles, FieldRolesOverride, IntentVocabulary, Metric,
    MetricSource, MetricSourceOverride, PreferredJoinPath, Sls, SlsOverrides,
};
use std::collections::HashMap;

fn root_priority(root_field: &str) -> u8 {
    if root_field.starts_with("query") {
        3
    } else if root_field.starts_with("get") {
        2
    } else if root_field.starts_with("batchGet") {
        1
    } else {
        0
    }
}

fn prefer_candidate_root(current: Option<&str>, candidate: &str) -> bool {
    let Some(current) = current else {
        return true;
    };
    root_priority(candidate) > root_priority(current)
}

pub fn derive_sls_from_schema(registry: &SchemaRegistry) -> Sls {
    let mut concepts = HashMap::new();

    for root_field in registry.root_fields() {
        if !root_field.starts_with("query")
            && !root_field.starts_with("get")
            && !root_field.starts_with("batchGet")
        {
            continue;
        }

        let Some(type_name) = registry.query_return_type(&root_field) else {
            continue;
        };

        let roles = registry.field_roles_for_root(&root_field);
        let concept = concepts
            .entry(type_name.to_lowercase())
            .or_insert_with(|| Concept {
                type_name: type_name.to_string(),
                synonyms: None,
                id_fields: None,
                canonical_path: None,
            });

        if prefer_candidate_root(concept.canonical_path.as_deref(), &root_field) {
            concept.canonical_path = Some(root_field.clone());
        }

        let id_fields = if roles.entity_key_fields.is_empty() {
            None
        } else {
            Some(roles.entity_key_fields)
        };
        merge_string_vec(&mut concept.id_fields, &id_fields);
    }

    Sls {
        concepts,
        metrics: None,
        field_roles: None,
        field_roles_by_type: HashMap::new(),
        field_roles_by_root: HashMap::new(),
        preferred_join_paths: Vec::new(),
        canonical_field_defaults: CanonicalFieldDefaults::default(),
        intent_vocabulary: IntentVocabulary::default(),
        policies: None,
        derived: Default::default(),
    }
}

#[cfg(test)]
fn generate_synonyms(type_name: &str) -> Vec<String> {
    let mut synonyms = Vec::new();
    let spaced = insert_spaces_before_caps(type_name);
    let spaced_lower = spaced.to_lowercase();
    synonyms.push(spaced_lower.clone());

    let parts: Vec<&str> = spaced_lower.split_whitespace().collect();
    if parts.len() > 1 {
        let without_first = parts[1..].join(" ");
        if !without_first.is_empty() && without_first != spaced_lower {
            synonyms.push(without_first);
        }
    }

    if let Some(last) = parts.last() {
        let last_str = (*last).to_string();
        if last_str != spaced_lower && !synonyms.contains(&last_str) {
            synonyms.push(last_str);
        }
    }

    let acronym: String = parts
        .iter()
        .filter_map(|part| part.chars().next())
        .collect::<String>()
        .to_uppercase();
    if acronym.len() > 1 && !synonyms.iter().any(|s| s.to_uppercase() == acronym) {
        synonyms.push(acronym);
    }

    synonyms.sort();
    synonyms.dedup();
    synonyms
}

#[cfg(test)]
fn insert_spaces_before_caps(s: &str) -> String {
    let mut result = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && ch.is_uppercase() {
            result.push(' ');
        }
        result.push(ch);
    }
    result
}

fn merge_string_vec(base: &mut Option<Vec<String>>, override_values: &Option<Vec<String>>) {
    let Some(values) = override_values else {
        return;
    };
    let mut merged = base.clone().unwrap_or_default();
    for value in values {
        if !merged
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(value))
        {
            merged.push(value.clone());
        }
    }
    *base = Some(merged);
}

fn merge_field_roles(base: &mut FieldRoles, override_roles: &FieldRolesOverride) {
    fn extend_unique(target: &mut Vec<String>, values: &Option<Vec<String>>) {
        let Some(values) = values else {
            return;
        };
        for value in values {
            if !target
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(value))
            {
                target.push(value.clone());
            }
        }
    }

    extend_unique(
        &mut base.entity_key_fields,
        &override_roles.entity_key_fields,
    );
    extend_unique(&mut base.entity_key_fields, &override_roles.id_fields);
    extend_unique(&mut base.label_fields, &override_roles.label_fields);
    extend_unique(&mut base.time_fields, &override_roles.time_fields);
    extend_unique(&mut base.numeric_fields, &override_roles.numeric_fields);
    extend_unique(&mut base.latitude_fields, &override_roles.latitude_fields);
    extend_unique(&mut base.longitude_fields, &override_roles.longitude_fields);
    extend_unique(
        &mut base.geo_object_fields,
        &override_roles.geo_object_fields,
    );
}

fn concept_canonical_path(
    concepts: &HashMap<String, Concept>,
    key_or_root: &str,
) -> Option<String> {
    if key_or_root.starts_with("query")
        || key_or_root.starts_with("get")
        || key_or_root.starts_with("batchGet")
    {
        return Some(key_or_root.to_string());
    }
    concepts
        .get(&key_or_root.to_lowercase())
        .and_then(|concept| concept.canonical_path.clone())
}

fn normalize_join_paths(
    concepts: &HashMap<String, Concept>,
    join_paths: &[PreferredJoinPath],
) -> Vec<PreferredJoinPath> {
    join_paths
        .iter()
        .filter_map(|join| {
            let from_root = concept_canonical_path(concepts, &join.from_root)?;
            let to_root = concept_canonical_path(concepts, &join.to_root)?;
            Some(PreferredJoinPath {
                from_root,
                to_root,
                strategy: join.strategy.clone(),
                description: join.description.clone(),
                left_time_field: join.left_time_field.clone(),
                right_time_field: join.right_time_field.clone(),
                left_id_field: join.left_id_field.clone(),
                right_id_field: join.right_id_field.clone(),
                max_window_minutes: join.max_window_minutes,
            })
        })
        .collect()
}

fn merge_metric_source(
    base: MetricSource,
    override_source: Option<MetricSourceOverride>,
) -> MetricSource {
    let Some(override_source) = override_source else {
        return base;
    };

    MetricSource {
        type_name: override_source.type_name.unwrap_or(base.type_name),
        filter: override_source.filter.or(base.filter),
        time_field: override_source.time_field.or(base.time_field),
        duration_field: override_source.duration_field.or(base.duration_field),
    }
}

fn build_metric_source(override_source: Option<MetricSourceOverride>) -> Option<MetricSource> {
    let override_source = override_source?;
    Some(MetricSource {
        type_name: override_source.type_name?,
        filter: override_source.filter,
        time_field: override_source.time_field,
        duration_field: override_source.duration_field,
    })
}

fn merge_canonical_defaults(
    base: &mut HashMap<String, HashMap<String, String>>,
    overrides: HashMap<String, HashMap<String, String>>,
) {
    for (scope, override_defaults) in overrides {
        let target = base.entry(scope).or_default();
        for (alias, field_name) in override_defaults {
            target.insert(alias, field_name);
        }
    }
}

fn extend_unique_string_vec(target: &mut Vec<String>, values: &[String]) {
    for value in values {
        if !target
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(value))
        {
            target.push(value.clone());
        }
    }
}

fn merge_intent_vocabulary(base: &mut IntentVocabulary, overrides: IntentVocabulary) {
    extend_unique_string_vec(&mut base.rank_desc, &overrides.rank_desc);
    extend_unique_string_vec(&mut base.rank_asc, &overrides.rank_asc);
    extend_unique_string_vec(&mut base.aggregate_count, &overrides.aggregate_count);
    extend_unique_string_vec(&mut base.aggregate_avg, &overrides.aggregate_avg);
    extend_unique_string_vec(&mut base.aggregate_sum, &overrides.aggregate_sum);
    extend_unique_string_vec(&mut base.compare, &overrides.compare);
    extend_unique_string_vec(&mut base.trend, &overrides.trend);
    extend_unique_string_vec(&mut base.temporal, &overrides.temporal);
    extend_unique_string_vec(&mut base.distance, &overrides.distance);
    extend_unique_string_vec(&mut base.membership, &overrides.membership);
    extend_unique_string_vec(&mut base.label_cues, &overrides.label_cues);
    extend_unique_string_vec(&mut base.entity_connectors, &overrides.entity_connectors);
    extend_unique_string_vec(&mut base.field_connectors, &overrides.field_connectors);
    extend_unique_string_vec(&mut base.filter_eq, &overrides.filter_eq);
    extend_unique_string_vec(&mut base.filter_contains, &overrides.filter_contains);
    extend_unique_string_vec(&mut base.group_by, &overrides.group_by);
}

pub fn merge_sls_overrides(mut base: Sls, overrides: SlsOverrides) -> Sls {
    for (concept_id, concept_override) in overrides.concepts {
        if let Some(existing) = base.concepts.get_mut(&concept_id) {
            if let Some(type_name) = concept_override.type_name {
                existing.type_name = type_name;
            }
            if let Some(canonical_path) = concept_override.canonical_path {
                existing.canonical_path = Some(canonical_path);
            }
            merge_string_vec(&mut existing.synonyms, &concept_override.synonyms);
            merge_string_vec(&mut existing.id_fields, &concept_override.id_fields);
        } else if let (Some(type_name), Some(canonical_path)) =
            (concept_override.type_name, concept_override.canonical_path)
        {
            base.concepts.insert(
                concept_id,
                Concept {
                    type_name,
                    synonyms: concept_override.synonyms,
                    id_fields: concept_override.id_fields,
                    canonical_path: Some(canonical_path),
                },
            );
        }
    }

    if let Some(field_roles_override) = overrides.field_roles {
        let mut base_roles = base.field_roles.unwrap_or_default();
        merge_field_roles(&mut base_roles, &field_roles_override);
        base.field_roles = Some(base_roles);
    }

    for (type_name, override_roles) in overrides.field_roles_by_type {
        let base_roles = base.field_roles_by_type.entry(type_name).or_default();
        merge_field_roles(base_roles, &override_roles);
    }

    for (root_field, override_roles) in overrides.field_roles_by_root {
        let base_roles = base.field_roles_by_root.entry(root_field).or_default();
        merge_field_roles(base_roles, &override_roles);
    }

    if let Some(metrics_override) = overrides.metrics {
        let mut merged_metrics = base.metrics.unwrap_or_default();
        for (name, metric_override) in metrics_override {
            let aggregation_override = metric_override.aggregation.or(metric_override.formula);
            if let Some(existing) = merged_metrics.get(&name).cloned() {
                merged_metrics.insert(
                    name,
                    Metric {
                        description: metric_override.description.or(existing.description),
                        aliases: {
                            let mut aliases = existing.aliases;
                            if let Some(override_aliases) = metric_override.aliases {
                                extend_unique_string_vec(&mut aliases, &override_aliases);
                            }
                            aliases
                        },
                        unit: metric_override.unit.or(existing.unit),
                        source: merge_metric_source(existing.source, metric_override.source),
                        aggregation: aggregation_override.or(existing.aggregation),
                    },
                );
                continue;
            }

            let Some(source) = build_metric_source(metric_override.source) else {
                continue;
            };
            let Some(aggregation) = aggregation_override else {
                continue;
            };
            merged_metrics.insert(
                name,
                Metric {
                    description: metric_override.description,
                    aliases: metric_override.aliases.unwrap_or_default(),
                    unit: metric_override.unit,
                    source,
                    aggregation: Some(aggregation),
                },
            );
        }
        base.metrics = Some(merged_metrics);
    }

    if !overrides.preferred_join_paths.is_empty() {
        base.preferred_join_paths =
            normalize_join_paths(&base.concepts, &overrides.preferred_join_paths);
    }

    if !overrides.canonical_field_defaults.by_type.is_empty()
        || !overrides.canonical_field_defaults.by_root.is_empty()
    {
        merge_canonical_defaults(
            &mut base.canonical_field_defaults.by_type,
            overrides.canonical_field_defaults.by_type,
        );
        merge_canonical_defaults(
            &mut base.canonical_field_defaults.by_root,
            overrides.canonical_field_defaults.by_root,
        );
    }

    if let Some(policies) = overrides.policies {
        base.policies = Some(policies);
    }

    merge_intent_vocabulary(&mut base.intent_vocabulary, overrides.intent_vocabulary);

    base
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sls::{ConceptOverride, MetricOverride, MetricSourceOverride};

    #[test]
    fn generates_synonyms_from_type_name() {
        let syns = generate_synonyms("OffshoreWindTurbine");
        assert!(syns.contains(&"offshore wind turbine".to_string()));
        assert!(syns.contains(&"wind turbine".to_string()));
        assert!(syns.contains(&"turbine".to_string()));
        assert!(syns.contains(&"OWT".to_string()));
    }

    #[test]
    fn merges_sparse_concept_overrides() {
        let registry = SchemaRegistry::new(include_str!("../schemas/consumer_schema.graphql"));
        let base = derive_sls_from_schema(&registry);
        let mut concepts = HashMap::new();
        concepts.insert(
            "offshorewindturbine".to_string(),
            ConceptOverride {
                type_name: None,
                synonyms: Some(vec!["WTG".to_string(), "unit".to_string()]),
                id_fields: None,
                canonical_path: None,
            },
        );
        let merged = merge_sls_overrides(
            base,
            SlsOverrides {
                concepts,
                ..Default::default()
            },
        );
        let syns = merged
            .concepts
            .get("offshorewindturbine")
            .and_then(|concept| concept.synonyms.clone())
            .unwrap_or_default();
        assert!(syns.iter().any(|syn| syn == "WTG"));
        assert!(syns.iter().any(|syn| syn == "unit"));
    }

    #[test]
    fn derive_prefers_query_root_as_canonical_path() {
        let registry = SchemaRegistry::new(include_str!("../schemas/consumer_schema.graphql"));
        let derived = derive_sls_from_schema(&registry);
        let canonical_path = derived
            .concepts
            .get("offshorewindfarm")
            .and_then(|concept| concept.canonical_path.as_deref());
        assert_eq!(canonical_path, Some("queryOffshoreWindFarm"));
    }

    #[test]
    fn derived_schema_concepts_do_not_emit_synonyms_as_trusted_sls_terms() {
        let registry = SchemaRegistry::new(include_str!("../schemas/consumer_schema.graphql"));
        let derived = derive_sls_from_schema(&registry);
        let synonyms = derived
            .concepts
            .get("offshorewindturbine")
            .and_then(|concept| concept.synonyms.as_ref());
        assert!(
            synonyms.is_none(),
            "schema-derived aliases should stay recall-only; explicit SLS overrides provide trusted synonyms"
        );
    }

    #[test]
    fn merge_allows_complete_metric_override() {
        let registry = SchemaRegistry::new(include_str!("../schemas/consumer_schema.graphql"));
        let base = derive_sls_from_schema(&registry);
        let mut metrics = HashMap::new();
        metrics.insert(
            "wind_speed".to_string(),
            MetricOverride {
                description: Some("Default wind speed metric".to_string()),
                source: Some(MetricSourceOverride {
                    type_name: Some("WeatherPrediction".to_string()),
                    filter: None,
                    time_field: Some("time".to_string()),
                    duration_field: None,
                }),
                aggregation: Some("avg(windSpeed10m)".to_string()),
                aliases: None,
                formula: None,
                unit: None,
            },
        );
        let merged = merge_sls_overrides(
            base,
            SlsOverrides {
                metrics: Some(metrics),
                ..Default::default()
            },
        );
        assert!(
            merged
                .metrics
                .as_ref()
                .and_then(|metrics| metrics.get("wind_speed"))
                .is_some()
        );
    }

    #[test]
    fn merge_allows_sparse_metric_override() {
        let mut metrics = HashMap::new();
        metrics.insert(
            "wind_speed".to_string(),
            Metric {
                description: Some("Base metric".to_string()),
                aliases: vec!["wind speed".to_string()],
                unit: Some("m/s".to_string()),
                source: MetricSource {
                    type_name: "WeatherPrediction".to_string(),
                    filter: Some(vec!["location = offshore".to_string()]),
                    time_field: Some("time".to_string()),
                    duration_field: None,
                },
                aggregation: Some("avg(windSpeed10m)".to_string()),
            },
        );
        let base = Sls {
            concepts: HashMap::new(),
            metrics: Some(metrics),
            field_roles: None,
            field_roles_by_type: HashMap::new(),
            field_roles_by_root: HashMap::new(),
            preferred_join_paths: Vec::new(),
            canonical_field_defaults: CanonicalFieldDefaults::default(),
            policies: None,
            derived: Default::default(),
            intent_vocabulary: IntentVocabulary::default(),
        };

        let mut overrides = HashMap::new();
        overrides.insert(
            "wind_speed".to_string(),
            MetricOverride {
                description: Some("Refined metric".to_string()),
                source: Some(MetricSourceOverride {
                    type_name: None,
                    filter: None,
                    time_field: Some("forecastTime".to_string()),
                    duration_field: None,
                }),
                aggregation: None,
                aliases: Some(vec!["forecast wind speed".to_string()]),
                formula: None,
                unit: None,
            },
        );

        let merged = merge_sls_overrides(
            base,
            SlsOverrides {
                metrics: Some(overrides),
                ..Default::default()
            },
        );
        let metric = merged
            .metrics
            .as_ref()
            .and_then(|metrics| metrics.get("wind_speed"))
            .expect("expected merged metric");

        assert_eq!(metric.description.as_deref(), Some("Refined metric"));
        assert_eq!(metric.source.type_name, "WeatherPrediction");
        assert_eq!(metric.source.time_field.as_deref(), Some("forecastTime"));
        assert_eq!(
            metric
                .source
                .filter
                .as_ref()
                .and_then(|filters| filters.first()),
            Some(&"location = offshore".to_string())
        );
        assert_eq!(metric.aggregation.as_deref(), Some("avg(windSpeed10m)"));
        assert!(
            metric.aliases.iter().any(|alias| alias == "wind speed")
                && metric
                    .aliases
                    .iter()
                    .any(|alias| alias == "forecast wind speed")
        );
        assert_eq!(metric.unit.as_deref(), Some("m/s"));
    }

    #[test]
    fn merge_uses_formula_for_new_metric_when_aggregation_is_missing() {
        let base = Sls {
            concepts: HashMap::new(),
            metrics: None,
            field_roles: None,
            field_roles_by_type: HashMap::new(),
            field_roles_by_root: HashMap::new(),
            preferred_join_paths: Vec::new(),
            canonical_field_defaults: CanonicalFieldDefaults::default(),
            policies: None,
            derived: Default::default(),
            intent_vocabulary: IntentVocabulary::default(),
        };
        let mut metrics = HashMap::new();
        metrics.insert(
            "wind_speed".to_string(),
            MetricOverride {
                description: Some("Derived from formula".to_string()),
                source: Some(MetricSourceOverride {
                    type_name: Some("WeatherPrediction".to_string()),
                    filter: None,
                    time_field: Some("time".to_string()),
                    duration_field: None,
                }),
                aggregation: None,
                aliases: None,
                formula: Some("avg(windSpeed10m)".to_string()),
                unit: Some("m/s".to_string()),
            },
        );

        let merged = merge_sls_overrides(
            base,
            SlsOverrides {
                metrics: Some(metrics),
                ..Default::default()
            },
        );
        let metric = merged
            .metrics
            .as_ref()
            .and_then(|all| all.get("wind_speed"))
            .expect("expected metric from formula override");
        assert_eq!(metric.aggregation.as_deref(), Some("avg(windSpeed10m)"));
        assert_eq!(metric.source.type_name, "WeatherPrediction");
        assert_eq!(metric.unit.as_deref(), Some("m/s"));
    }

    #[test]
    fn merge_canonical_field_defaults_preserves_existing_entries() {
        let mut by_type = HashMap::new();
        by_type.insert(
            "WeatherPrediction".to_string(),
            HashMap::from([("wind speed".to_string(), "windSpeed10m".to_string())]),
        );
        let base = Sls {
            concepts: HashMap::new(),
            metrics: None,
            field_roles: None,
            field_roles_by_type: HashMap::new(),
            field_roles_by_root: HashMap::new(),
            preferred_join_paths: Vec::new(),
            canonical_field_defaults: CanonicalFieldDefaults {
                by_type,
                by_root: HashMap::new(),
            },
            policies: None,
            derived: Default::default(),
            intent_vocabulary: IntentVocabulary::default(),
        };

        let merged = merge_sls_overrides(
            base,
            SlsOverrides {
                canonical_field_defaults: CanonicalFieldDefaults {
                    by_type: HashMap::from([(
                        "WeatherPrediction".to_string(),
                        HashMap::from([("forecast time".to_string(), "time".to_string())]),
                    )]),
                    by_root: HashMap::new(),
                },
                ..Default::default()
            },
        );
        let defaults = merged
            .canonical_field_defaults
            .by_type
            .get("WeatherPrediction")
            .expect("expected merged defaults");
        assert_eq!(
            defaults.get("wind speed").map(String::as_str),
            Some("windSpeed10m")
        );
        assert_eq!(
            defaults.get("forecast time").map(String::as_str),
            Some("time")
        );
    }
}
