use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct Sls {
    pub concepts: HashMap<String, Concept>,
    pub metrics: Option<HashMap<String, Metric>>,
    pub field_roles: Option<FieldRoles>,
    #[serde(default)]
    pub field_roles_by_type: HashMap<String, FieldRoles>,
    #[serde(default)]
    pub field_roles_by_root: HashMap<String, FieldRoles>,
    #[serde(default)]
    pub preferred_join_paths: Vec<PreferredJoinPath>,
    #[serde(default)]
    pub canonical_field_defaults: CanonicalFieldDefaults,
    #[serde(default)]
    pub intent_vocabulary: IntentVocabulary,
    pub policies: Option<Policies>,
    #[serde(skip)]
    pub derived: SlsDerived,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct SlsOverrides {
    #[serde(default)]
    pub concepts: HashMap<String, ConceptOverride>,
    pub metrics: Option<HashMap<String, MetricOverride>>,
    pub field_roles: Option<FieldRolesOverride>,
    #[serde(default)]
    pub field_roles_by_type: HashMap<String, FieldRolesOverride>,
    #[serde(default)]
    pub field_roles_by_root: HashMap<String, FieldRolesOverride>,
    #[serde(default)]
    pub preferred_join_paths: Vec<PreferredJoinPath>,
    #[serde(default)]
    pub canonical_field_defaults: CanonicalFieldDefaults,
    #[serde(default)]
    pub intent_vocabulary: IntentVocabulary,
    pub policies: Option<Policies>,
}

#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct SlsDerived {
    // normalized concept/synonym token -> canonical root path
    pub concept_token_to_root: HashMap<String, String>,
    // canonical roots that must include time-window constraints
    pub required_time_window_roots: HashSet<String>,
    // normalized unordered root-pair keys that are explicitly preferred for joins
    pub preferred_join_pair_keys: HashSet<String>,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct PreferredJoinPath {
    pub from_root: String,
    pub to_root: String,
    pub strategy: Option<String>,
    pub description: Option<String>,
    pub left_time_field: Option<String>,
    pub right_time_field: Option<String>,
    pub left_id_field: Option<String>,
    pub right_id_field: Option<String>,
    pub max_window_minutes: Option<i64>,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct Concept {
    #[serde(rename = "type")]
    pub type_name: String,
    pub synonyms: Option<Vec<String>>,
    pub id_fields: Option<Vec<String>>,
    pub canonical_path: Option<String>,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct ConceptOverride {
    #[serde(rename = "type")]
    pub type_name: Option<String>,
    pub synonyms: Option<Vec<String>>,
    pub id_fields: Option<Vec<String>>,
    pub canonical_path: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct Metric {
    pub description: Option<String>,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub unit: Option<String>,
    pub source: MetricSource,
    pub aggregation: Option<String>,
}

#[derive(Debug, Deserialize, Clone, Default)]
#[allow(dead_code)]
pub struct MetricOverride {
    pub description: Option<String>,
    #[serde(default)]
    pub aliases: Option<Vec<String>>,
    pub source: Option<MetricSourceOverride>,
    pub aggregation: Option<String>,
    pub formula: Option<String>,
    pub unit: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct MetricSource {
    #[serde(rename = "type")]
    pub type_name: String,
    pub filter: Option<Vec<String>>,
    pub time_field: Option<String>,
    pub duration_field: Option<String>,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct MetricSourceOverride {
    #[serde(rename = "type")]
    pub type_name: Option<String>,
    pub filter: Option<Vec<String>>,
    pub time_field: Option<String>,
    pub duration_field: Option<String>,
}

#[derive(Debug, Deserialize, Clone, Default)]
#[allow(dead_code)]
pub struct CanonicalFieldDefaults {
    #[serde(default)]
    pub by_type: HashMap<String, HashMap<String, String>>,
    #[serde(default)]
    pub by_root: HashMap<String, HashMap<String, String>>,
}

#[derive(Debug, Deserialize, Clone, Default, PartialEq, Eq)]
#[allow(dead_code)]
pub struct IntentVocabulary {
    #[serde(default)]
    pub rank_desc: Vec<String>,
    #[serde(default)]
    pub rank_asc: Vec<String>,
    #[serde(default)]
    pub aggregate_count: Vec<String>,
    #[serde(default)]
    pub aggregate_avg: Vec<String>,
    #[serde(default)]
    pub aggregate_sum: Vec<String>,
    #[serde(default)]
    pub compare: Vec<String>,
    #[serde(default)]
    pub trend: Vec<String>,
    #[serde(default)]
    pub temporal: Vec<String>,
    #[serde(default)]
    pub distance: Vec<String>,
    #[serde(default)]
    pub membership: Vec<String>,
    #[serde(default)]
    pub label_cues: Vec<String>,
    #[serde(default)]
    pub entity_connectors: Vec<String>,
    #[serde(default)]
    pub field_connectors: Vec<String>,
    #[serde(default)]
    pub filter_eq: Vec<String>,
    #[serde(default)]
    pub filter_contains: Vec<String>,
    #[serde(default)]
    pub group_by: Vec<String>,
}

#[derive(Debug, Deserialize, Clone, Default)]
#[allow(dead_code)]
pub struct FieldRoles {
    #[serde(default)]
    pub entity_key_fields: Vec<String>,
    #[serde(default)]
    pub label_fields: Vec<String>,
    #[serde(default)]
    pub time_fields: Vec<String>,
    #[serde(default)]
    pub numeric_fields: Vec<String>,
    #[serde(default)]
    pub latitude_fields: Vec<String>,
    #[serde(default)]
    pub longitude_fields: Vec<String>,
    #[serde(default)]
    pub geo_object_fields: Vec<String>,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct FieldRolesOverride {
    #[serde(default)]
    pub entity_key_fields: Option<Vec<String>>,
    #[serde(default)]
    pub id_fields: Option<Vec<String>>,
    #[serde(default)]
    pub label_fields: Option<Vec<String>>,
    #[serde(default)]
    pub time_fields: Option<Vec<String>>,
    #[serde(default)]
    pub numeric_fields: Option<Vec<String>>,
    #[serde(default)]
    pub latitude_fields: Option<Vec<String>>,
    #[serde(default)]
    pub longitude_fields: Option<Vec<String>>,
    #[serde(default)]
    pub geo_object_fields: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct Policies {
    pub limits: Option<Limits>,
    pub fallback: Option<FallbackPolicies>,
    #[serde(default)]
    pub field_allowlists: HashMap<String, Vec<String>>,
    pub aggregation: Option<AggregationPolicies>,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct Limits {
    pub max_depth: Option<u32>,
    pub max_rows: Option<u32>,
    pub max_complexity: Option<u32>,
    pub require_time_window_for: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Clone, Default)]
#[allow(dead_code)]
pub struct AggregationPolicies {
    pub max_group_by_fields: Option<u32>,
    pub max_groups: Option<u32>,
    #[serde(default)]
    pub require_time_window_for_metrics: Vec<String>,
}

#[derive(Debug, Deserialize, Clone, Default)]
#[allow(dead_code)]
pub struct FallbackPolicies {
    pub simple_fetch: Option<SimpleFetchFallbackPolicy>,
}

#[derive(Debug, Deserialize, Clone, Default)]
#[allow(dead_code)]
pub struct SimpleFetchFallbackPolicy {
    #[serde(default)]
    pub allow_explicit_field_constraints: bool,
    #[serde(default)]
    pub allow_compact_identifier_lookup: bool,
    #[serde(default)]
    pub deny_intents: Vec<String>,
}

fn normalize_token(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

fn tokenize_text(s: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            current.push(ch.to_ascii_lowercase());
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn token_sequence_contains_term(message_tokens: &[String], term: &str) -> bool {
    let term_tokens = tokenize_text(term);
    if term_tokens.is_empty() || term_tokens.len() > message_tokens.len() {
        return false;
    }
    message_tokens
        .windows(term_tokens.len())
        .any(|window| window == term_tokens.as_slice())
}

fn pair_key(a: &str, b: &str) -> String {
    let mut roots = [a.to_lowercase(), b.to_lowercase()];
    roots.sort();
    format!("{}|{}", roots[0], roots[1])
}

fn build_derived(sls: &Sls) -> SlsDerived {
    let mut concept_token_to_root = HashMap::new();
    for (concept_name, concept) in &sls.concepts {
        let Some(root) = concept.canonical_path.as_ref() else {
            continue;
        };
        let root = root.to_lowercase();
        let mut tokens = vec![concept_name.clone()];
        if let Some(syn) = &concept.synonyms {
            tokens.extend(syn.clone());
        }
        for t in tokens {
            let n = normalize_token(&t);
            if n.is_empty() {
                continue;
            }
            concept_token_to_root
                .entry(n)
                .or_insert_with(|| root.clone());
        }
    }

    let mut required_time_window_roots = HashSet::new();
    if let Some(terms) = sls
        .policies
        .as_ref()
        .and_then(|p| p.limits.as_ref())
        .and_then(|l| l.require_time_window_for.as_ref())
    {
        for term in terms {
            let n = normalize_token(term);
            if n.is_empty() {
                continue;
            }
            let singular = n.strip_suffix('s').unwrap_or(&n);
            if let Some(root) = concept_token_to_root.get(&n) {
                required_time_window_roots.insert(root.clone());
            } else if let Some(root) = concept_token_to_root.get(singular) {
                required_time_window_roots.insert(root.clone());
            } else if n.starts_with("query") {
                required_time_window_roots.insert(term.to_lowercase());
            }
        }
    }

    let mut preferred_join_pair_keys = HashSet::new();
    for join in &sls.preferred_join_paths {
        preferred_join_pair_keys.insert(pair_key(&join.from_root, &join.to_root));
    }

    SlsDerived {
        concept_token_to_root,
        required_time_window_roots,
        preferred_join_pair_keys,
    }
}

impl Sls {
    fn intent_terms(&self, intent: &str) -> &[String] {
        match intent {
            "rank_desc" => &self.intent_vocabulary.rank_desc,
            "rank_asc" => &self.intent_vocabulary.rank_asc,
            "aggregate_count" => &self.intent_vocabulary.aggregate_count,
            "aggregate_avg" => &self.intent_vocabulary.aggregate_avg,
            "aggregate_sum" => &self.intent_vocabulary.aggregate_sum,
            "compare" => &self.intent_vocabulary.compare,
            "trend" => &self.intent_vocabulary.trend,
            "temporal" => &self.intent_vocabulary.temporal,
            "distance" => &self.intent_vocabulary.distance,
            "membership" => &self.intent_vocabulary.membership,
            "label_cues" => &self.intent_vocabulary.label_cues,
            "entity_connectors" => &self.intent_vocabulary.entity_connectors,
            "field_connectors" => &self.intent_vocabulary.field_connectors,
            "filter_eq" => &self.intent_vocabulary.filter_eq,
            "filter_contains" => &self.intent_vocabulary.filter_contains,
            "group_by" => &self.intent_vocabulary.group_by,
            _ => &[],
        }
    }

    fn message_mentions_any_terms(&self, message: &str, terms: &[String]) -> bool {
        let message_tokens = tokenize_text(message);
        if message_tokens.is_empty() {
            return false;
        }
        terms
            .iter()
            .any(|term| token_sequence_contains_term(&message_tokens, term))
    }

    pub fn message_mentions_intent(&self, message: &str, intent: &str) -> bool {
        self.message_mentions_any_terms(message, self.intent_terms(intent))
    }

    pub fn simple_fetch_fallback_policy(&self) -> Option<&SimpleFetchFallbackPolicy> {
        self.policies
            .as_ref()
            .and_then(|policies| policies.fallback.as_ref())
            .and_then(|fallback| fallback.simple_fetch.as_ref())
    }

    pub fn simple_fetch_fallback_denied_intent(&self, message: &str) -> Option<String> {
        let policy = self.simple_fetch_fallback_policy()?;
        policy
            .deny_intents
            .iter()
            .find(|intent| self.message_mentions_any_terms(message, self.intent_terms(intent)))
            .cloned()
    }

    pub fn message_mentions_concept(&self, message: &str, concept_key: &str) -> bool {
        let Some(concept) = self.concepts.get(concept_key) else {
            return false;
        };
        let message_tokens = tokenize_text(message);
        if message_tokens.is_empty() {
            return false;
        }
        let mut tokens = vec![concept_key.to_string()];
        if let Some(syn) = &concept.synonyms {
            tokens.extend(syn.clone());
        }
        tokens
            .into_iter()
            .any(|term| token_sequence_contains_term(&message_tokens, &term))
    }

    pub fn is_preferred_join_pair(&self, left_root: &str, right_root: &str) -> bool {
        self.derived
            .preferred_join_pair_keys
            .contains(&pair_key(left_root, right_root))
    }

    pub fn preferred_join_for_pair(
        &self,
        left_root: &str,
        right_root: &str,
    ) -> Option<&PreferredJoinPath> {
        self.preferred_join_paths.iter().find(|p| {
            (p.from_root.eq_ignore_ascii_case(left_root)
                && p.to_root.eq_ignore_ascii_case(right_root))
                || (p.from_root.eq_ignore_ascii_case(right_root)
                    && p.to_root.eq_ignore_ascii_case(left_root))
        })
    }

    pub fn join_paths_prompt_block(&self) -> String {
        if self.preferred_join_paths.is_empty() {
            return String::new();
        }
        let payload = serde_json::json!({
            "preferred_join_paths": self.preferred_join_paths.iter().map(|p| serde_json::json!({
                "from_root": p.from_root,
                "to_root": p.to_root,
                "strategy": p.strategy,
                "left_time_field": p.left_time_field,
                "right_time_field": p.right_time_field,
                "left_id_field": p.left_id_field,
                "right_id_field": p.right_id_field,
                "max_window_minutes": p.max_window_minutes,
                "description": p.description
            })).collect::<Vec<_>>()
        });
        format!(
            "SLS preferred join paths (JSON, prioritize these chains):\n{}\n",
            serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".to_string())
        )
    }

    pub fn metrics_prompt_block(&self) -> String {
        let Some(metrics) = self.metrics.as_ref() else {
            return String::new();
        };
        if metrics.is_empty() {
            return String::new();
        }
        let mut keys = metrics.keys().cloned().collect::<Vec<_>>();
        keys.sort();
        let payload = keys
            .into_iter()
            .filter_map(|name| {
                metrics.get(&name).map(|metric| {
                    serde_json::json!({
                        "name": name,
                        "description": metric.description,
                        "aliases": metric.aliases,
                        "unit": metric.unit,
                        "aggregation": metric.aggregation,
                        "source_type": metric.source.type_name,
                        "time_field": metric.source.time_field,
                        "duration_field": metric.source.duration_field,
                        "filter": metric.source.filter
                    })
                })
            })
            .collect::<Vec<_>>();
        format!(
            "SLS metrics catalog (JSON, use as {{\"op\":\"metric\",\"name\":\"<metric>\"}}):\n{}\n",
            serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "[]".to_string())
        )
    }

    pub fn canonical_fields_prompt_block(&self) -> String {
        if self.canonical_field_defaults.by_type.is_empty()
            && self.canonical_field_defaults.by_root.is_empty()
        {
            return String::new();
        }
        let payload = serde_json::json!({
            "by_type": self.canonical_field_defaults.by_type,
            "by_root": self.canonical_field_defaults.by_root
        });
        format!(
            "SLS canonical field defaults (JSON, prefer these exact schema fields for generic domain terms):\n{}\n",
            serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".to_string())
        )
    }

    pub fn intent_vocabulary_prompt_block(&self) -> String {
        let payload = serde_json::json!({
            "rank_desc": self.intent_vocabulary.rank_desc,
            "rank_asc": self.intent_vocabulary.rank_asc,
            "aggregate_count": self.intent_vocabulary.aggregate_count,
            "aggregate_avg": self.intent_vocabulary.aggregate_avg,
            "aggregate_sum": self.intent_vocabulary.aggregate_sum,
            "compare": self.intent_vocabulary.compare,
            "trend": self.intent_vocabulary.trend,
            "temporal": self.intent_vocabulary.temporal,
            "distance": self.intent_vocabulary.distance,
            "membership": self.intent_vocabulary.membership,
            "label_cues": self.intent_vocabulary.label_cues,
            "entity_connectors": self.intent_vocabulary.entity_connectors,
            "field_connectors": self.intent_vocabulary.field_connectors,
            "filter_eq": self.intent_vocabulary.filter_eq,
            "filter_contains": self.intent_vocabulary.filter_contains,
            "group_by": self.intent_vocabulary.group_by,
        });
        format!(
            "SLS intent vocabulary (JSON, use these terms to infer operator shape):\n{}\n",
            serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".to_string())
        )
    }
}

#[allow(dead_code)]
pub fn load_sls(path: impl AsRef<Path>) -> anyhow::Result<Sls> {
    let content = fs::read_to_string(path)?;
    let mut sls = serde_yaml::from_str::<Sls>(&content)?;
    sls.derived = build_derived(&sls);
    Ok(sls)
}

pub fn load_sls_overrides(path: impl AsRef<Path>) -> anyhow::Result<SlsOverrides> {
    let content = fs::read_to_string(path)?;
    let overrides = serde_yaml::from_str::<SlsOverrides>(&content)?;
    Ok(overrides)
}

/// Load manual SLS and merge with auto-derived base from schema
///
/// Auto-derivation ensures all schema types are included as concepts,
/// while manual SLS overrides add domain-specific refinements.
pub fn load_sls_merged(
    schema_registry: &crate::schema_registry::SchemaRegistry,
    manual_sls_path: impl AsRef<Path>,
) -> anyhow::Result<Sls> {
    let auto_sls = crate::sls_derive::derive_sls_from_schema(schema_registry);
    let overrides = if manual_sls_path.as_ref().exists() {
        load_sls_overrides(&manual_sls_path)?
    } else {
        SlsOverrides::default()
    };
    let mut sls = crate::sls_derive::merge_sls_overrides(auto_sls, overrides);
    sls.derived = build_derived(&sls);
    Ok(sls)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_sls() -> Sls {
        let mut concepts = HashMap::new();
        concepts.insert(
            "farm".to_string(),
            Concept {
                type_name: "OffshoreWindFarm".to_string(),
                synonyms: Some(vec!["wind farm".to_string()]),
                id_fields: None,
                canonical_path: Some("queryOffshoreWindFarm".to_string()),
            },
        );
        Sls {
            concepts,
            metrics: None,
            field_roles: None,
            field_roles_by_type: HashMap::new(),
            field_roles_by_root: HashMap::new(),
            preferred_join_paths: Vec::new(),
            canonical_field_defaults: CanonicalFieldDefaults::default(),
            intent_vocabulary: IntentVocabulary {
                membership: vec!["in".to_string(), "part of".to_string()],
                ..IntentVocabulary::default()
            },
            policies: None,
            derived: SlsDerived::default(),
        }
    }

    #[test]
    fn intent_matching_uses_words_not_substrings() {
        let sls = test_sls();

        assert!(
            sls.message_mentions_intent("List turbines in Wind Farm 3", "membership"),
            "expected standalone `in` to match membership intent"
        );
        assert!(
            sls.message_mentions_intent("Which turbines are part of Wind Farm 3?", "membership"),
            "expected phrase term to match membership intent"
        );
        assert!(
            !sls.message_mentions_intent("Show wind speed forecasts", "membership"),
            "`in` inside `wind` must not match membership intent"
        );
    }

    #[test]
    fn concept_matching_uses_phrase_tokens() {
        let sls = test_sls();

        assert!(
            sls.message_mentions_concept("List turbines in wind farm 3", "farm"),
            "expected concept phrase synonym to match"
        );
        assert!(
            !sls.message_mentions_concept("Show wind speed forecasts", "farm"),
            "concept matching should not match partial words"
        );
    }
}
