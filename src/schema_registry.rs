#![allow(clippy::needless_raw_string_hashes)]

use crate::domain_config::{DomainConfig, FieldRoleSet, build_domain_config};
use crate::error::{PipelineError, PipelineResult};
use crate::sls::{IntentVocabulary, Sls};
use graphql_parser::query::{
    Definition as QueryDefinition, OperationDefinition, Selection, Value as QueryValue, parse_query,
};
use graphql_parser::schema::{Definition, Field, Type, TypeDefinition, parse_schema};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use tracing::info;

#[derive(Clone)]
enum DefType {
    Object,
    Input,
    Enum,
    Interface,
    Other,
}

#[derive(Clone)]
struct SchemaEntry {
    def_type: DefType,
    content: String,
}

#[derive(Clone, Copy, Debug, Default)]
struct QueryIntent {
    aggregate_like: bool,
    compare_like: bool,
    rank_like: bool,
    trend_like: bool,
    time_like: bool,
}

#[derive(Clone, Debug, Default)]
struct QueryIntentSignals {
    mention_count: usize,
    numeric_literal_count: usize,
    has_metric_field_phrase: bool,
    has_temporal_literal: bool,
    has_explicit_aggregate_operator: bool,
    has_grouping_shape: bool,
    top_numeric_root_count: usize,
    top_time_root_count: usize,
    top_relation_root_count: usize,
    top_identifier_filter_root_count: usize,
    top_numeric_overlap_count: usize,
    top_time_overlap_count: usize,
}

#[derive(Clone, Debug)]
struct RootIntentEvidence {
    root: String,
    base_score: i32,
    has_filter_fields: bool,
    has_label_fields: bool,
    has_numeric_fields: bool,
    has_time_fields: bool,
    has_relation_fields: bool,
    has_identifier_filter_fields: bool,
    has_time_filter_fields: bool,
    numeric_overlap_count: usize,
    time_overlap_count: usize,
}

impl QueryIntent {
    fn infer(signals: &QueryIntentSignals) -> Self {
        let compare_like = signals.mention_count >= 2
            && (signals.top_numeric_root_count > 0 || signals.top_identifier_filter_root_count > 0);
        let rank_like = signals.has_metric_field_phrase
            && signals.numeric_literal_count > 0
            && signals.top_numeric_root_count > 0;
        let time_like = signals.top_time_root_count > 0
            && (signals.top_time_overlap_count > 0 || signals.has_temporal_literal);
        let trend_like = time_like
            && signals.top_numeric_root_count > 0
            && (signals.top_numeric_overlap_count > 0 || signals.has_metric_field_phrase);
        let aggregate_like = signals.has_explicit_aggregate_operator
            || rank_like
            || (compare_like && signals.top_numeric_root_count > 0)
            || (signals.top_relation_root_count > 0
                && (signals.has_grouping_shape || signals.numeric_literal_count > 0));

        Self {
            aggregate_like,
            compare_like,
            rank_like,
            trend_like,
            time_like,
        }
    }

    fn describe(self) -> String {
        let mut labels = Vec::new();
        if self.compare_like {
            labels.push("compare");
        }
        if self.aggregate_like {
            labels.push("aggregate");
        }
        if self.rank_like {
            labels.push("rank");
        }
        if self.trend_like {
            labels.push("trend");
        }
        if self.time_like && !self.trend_like {
            labels.push("time");
        }
        if labels.is_empty() {
            "lookup/list".to_string()
        } else {
            labels.join(" + ")
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlannerRetrievalBudget {
    pub root_limit: usize,
    pub field_limit: usize,
    pub entity_resolution_limit: usize,
}

fn push_unique_string(out: &mut Vec<String>, value: String) {
    if !out.iter().any(|existing| existing == &value) {
        out.push(value);
    }
}

fn ordered_unique_candidates(candidates: &[String]) -> Vec<String> {
    let mut ordered = Vec::new();
    for candidate in candidates {
        if !ordered.iter().any(|existing| existing == candidate) {
            ordered.push(candidate.clone());
        }
    }
    ordered
}

fn partition_candidates_by_membership(
    candidates: &[String],
    preferred: &HashSet<String>,
) -> Vec<String> {
    let mut preferred_out = Vec::new();
    let mut fallback_out = Vec::new();
    for candidate in ordered_unique_candidates(candidates) {
        if preferred.contains(&candidate) {
            preferred_out.push(candidate);
        } else {
            fallback_out.push(candidate);
        }
    }
    preferred_out.extend(fallback_out);
    preferred_out
}

fn score_candidate_text(tokens: &[String], candidate: &str) -> i32 {
    let candidate_lower = candidate.to_ascii_lowercase();
    let mut score = 0i32;
    for token in tokens {
        if candidate_lower == *token {
            score += 50;
        } else if candidate_lower.contains(token) {
            score += 20;
        }
    }
    score
}

fn count_query_mentions(query: &str) -> usize {
    let mut mentions = HashSet::new();

    let quoted = regex::Regex::new(r#""([^"]+)""#).expect("quoted mention regex");
    for caps in quoted.captures_iter(query) {
        if let Some(value) = caps.get(1).map(|m| m.as_str().trim())
            && !value.is_empty()
        {
            mentions.insert(value.to_string());
        }
    }

    let code_like = regex::Regex::new(
        r"\b[A-Z]{1,}[A-Z0-9]*(?:[-_][A-Z0-9]+)+\b|\b[A-Z]{2,}\d+\b|\b[A-Z]\d+\b",
    )
    .expect("code-like mention regex");
    for m in code_like.find_iter(query) {
        mentions.insert(m.as_str().trim().to_string());
    }

    mentions.len()
}

fn count_numeric_literals(query: &str) -> usize {
    query
        .split(|c: char| !c.is_ascii_digit())
        .filter(|token| !token.is_empty())
        .count()
}

fn has_temporal_literal(query: &str) -> bool {
    let iso_date = regex::Regex::new(r"\b\d{4}-\d{2}-\d{2}\b").expect("iso date regex");
    let slash_date = regex::Regex::new(r"\b\d{1,2}/\d{1,2}/\d{2,4}\b").expect("slash date regex");
    let clock_time = regex::Regex::new(r"\b\d{1,2}:\d{2}(?::\d{2})?\b").expect("clock time regex");
    iso_date.is_match(query) || slash_date.is_match(query) || clock_time.is_match(query)
}

fn extract_field_phrase_after_by(query: &str) -> Option<String> {
    let re = regex::Regex::new(r#"(?i)\bby\s+([A-Za-z_][A-Za-z0-9_]*)"#).expect("by-field regex");
    re.captures(query)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str().trim().to_string())
        .filter(|value| !value.is_empty())
}

fn overlap_count(tokens: &[String], token_phrases: &[String], candidates: &[String]) -> usize {
    candidates
        .iter()
        .filter(|candidate| {
            let lower = candidate.to_ascii_lowercase();
            tokens
                .iter()
                .any(|token| lower == *token || lower.contains(token))
                || token_phrases.iter().any(|phrase| lower.contains(phrase))
        })
        .count()
}

fn schema_type_tokens(type_name: &str) -> Vec<String> {
    let token_re = regex::Regex::new(r"[A-Z][a-z0-9]*|[a-z0-9]+").expect("schema type token regex");
    token_re
        .find_iter(type_name)
        .map(|m| m.as_str().to_ascii_lowercase())
        .collect()
}

fn generated_concept_aliases(type_name: &str) -> Vec<String> {
    let mut aliases = Vec::new();
    let tokens = schema_type_tokens(type_name);
    if tokens.is_empty() {
        return aliases;
    }

    aliases.push(tokens.join(" "));
    if tokens.len() > 1 {
        aliases.push(tokens[1..].join(" "));
    }
    aliases.push(tokens[tokens.len() - 1].clone());

    let acronym = tokens
        .iter()
        .filter_map(|token| token.chars().next())
        .collect::<String>()
        .to_uppercase();
    if acronym.len() > 1 {
        aliases.push(acronym);
    }

    aliases.retain(|alias| !alias.trim().is_empty());
    aliases.sort();
    aliases.dedup();
    aliases
}

fn push_unique_alias(out: &mut Vec<String>, alias: &str) {
    let value = alias.trim();
    if value.is_empty() {
        return;
    }
    if out
        .iter()
        .any(|existing| existing.eq_ignore_ascii_case(value))
    {
        return;
    }
    out.push(value.to_string());
}

fn build_concept_aliases_by_type(
    query_return_types: &HashMap<String, String>,
    sls: Option<&Sls>,
) -> HashMap<String, Vec<String>> {
    let explicit_by_type = build_explicit_concept_aliases_by_type(query_return_types, sls);
    let mut by_type = HashMap::new();
    let mut type_names = query_return_types.values().cloned().collect::<Vec<_>>();
    type_names.sort();
    type_names.dedup();

    for type_name in type_names {
        let mut aliases = generated_concept_aliases(&type_name);
        if let Some(explicit_aliases) = explicit_by_type.get(&type_name.to_ascii_lowercase()) {
            for alias in explicit_aliases {
                push_unique_alias(&mut aliases, alias);
            }
        }
        aliases.sort();
        aliases.dedup();
        by_type.insert(type_name.to_ascii_lowercase(), aliases);
    }

    by_type
}

fn build_explicit_concept_aliases_by_type(
    query_return_types: &HashMap<String, String>,
    sls: Option<&Sls>,
) -> HashMap<String, Vec<String>> {
    let Some(sls) = sls else {
        return HashMap::new();
    };
    let known_types = query_return_types
        .values()
        .map(|type_name| type_name.to_ascii_lowercase())
        .collect::<HashSet<_>>();
    let mut by_type: HashMap<String, Vec<String>> = HashMap::new();
    for (concept_key, concept) in &sls.concepts {
        let type_key = concept.type_name.to_ascii_lowercase();
        if !known_types.contains(&type_key) {
            continue;
        }
        let aliases = by_type.entry(type_key).or_default();
        if !concept_key.eq_ignore_ascii_case(&concept.type_name)
            && !concept_key.eq_ignore_ascii_case(&concept.type_name.replace('_', ""))
        {
            push_unique_alias(aliases, concept_key);
        }
        if let Some(synonyms) = &concept.synonyms {
            for synonym in synonyms {
                push_unique_alias(aliases, synonym);
            }
        }
        aliases.sort();
        aliases.dedup();
    }
    by_type
}

const MIN_RELATION_QUERY_RELEVANCE: i32 = 50;

#[derive(Clone, Debug)]
pub struct InputTypeRef {
    pub name: String,
    pub is_list: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchemaSource {
    StaticFile,
    LocalFile,
    LiveIntrospection,
    CachedIntrospection,
}

impl SchemaSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::StaticFile => "static_file",
            Self::LocalFile => "local_file",
            Self::LiveIntrospection => "live_introspection",
            Self::CachedIntrospection => "cached_introspection",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub enum RetrievalConfidence {
    High,
    Medium,
    Low,
}

impl RetrievalConfidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct QueryRootMatch {
    pub root: String,
    pub score: i32,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct QueryRootRetrievalProfile {
    pub matches: Vec<QueryRootMatch>,
    pub top_score: i32,
    pub runner_up_score: i32,
    pub competitive_root_count: usize,
    pub confidence: RetrievalConfidence,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct RetrievedRootSlice {
    pub root: String,
    pub score: i32,
    pub capability_evidence: Vec<String>,
    pub return_type: String,
    pub concept_aliases: Vec<String>,
    pub key_fields: Vec<String>,
    pub intent_fields: Vec<String>,
    pub default_scalar_fields: Vec<String>,
    pub numeric_fields: Vec<String>,
    pub time_fields: Vec<String>,
    pub relation_fields: Vec<String>,
    pub filter_fields: Vec<String>,
    pub identifier_filter_fields: Vec<String>,
    pub time_filter_fields: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct SchemaRetrievalSlice {
    pub intent: String,
    pub profile: QueryRootRetrievalProfile,
    pub roots: Vec<RetrievedRootSlice>,
}

#[derive(Clone)]
pub struct SchemaRegistry {
    definitions: HashMap<String, SchemaEntry>,
    valid_root_fields: HashSet<String>,
    query_return_types: HashMap<String, String>,
    query_filter_inputs: HashMap<String, String>,
    query_order_inputs: HashMap<String, String>,
    query_arg_type_refs: HashMap<String, HashMap<String, InputTypeRef>>,
    object_fields: HashMap<String, HashSet<String>>,
    object_field_types: HashMap<String, HashMap<String, String>>,
    object_field_type_refs: HashMap<String, HashMap<String, InputTypeRef>>,
    input_fields: HashMap<String, HashSet<String>>,
    input_field_type_refs: HashMap<String, HashMap<String, InputTypeRef>>,
    enum_values_map: HashMap<String, Vec<String>>,
    concept_aliases_by_type: HashMap<String, Vec<String>>,
    explicit_concept_aliases_by_type: HashMap<String, Vec<String>>,
    intent_vocabulary: IntentVocabulary,
    domain_config: DomainConfig,
    #[allow(dead_code)]
    schema_source: SchemaSource,
}

impl SchemaRegistry {
    pub fn planner_retrieval_budget(&self, query: &str) -> PlannerRetrievalBudget {
        let intent = self.infer_query_intent(query);
        let query_tokens = Self::query_tokens(query);
        let token_count = query_tokens.len();
        let quoted_span_count = query.matches('"').count() / 2;
        let identifier_like_token_count = query_tokens
            .iter()
            .filter(|token| {
                token.chars().any(|ch| ch.is_ascii_digit())
                    || token.contains('-')
                    || token.contains('_')
                    || token.contains(':')
            })
            .count();
        let simple_lookup_like = !intent.aggregate_like
            && !intent.compare_like
            && !intent.rank_like
            && !intent.trend_like
            && !intent.time_like
            && token_count <= 4;

        let mut root_limit: usize = if simple_lookup_like { 3 } else { 4 };
        let mut field_limit: usize = if simple_lookup_like { 6 } else { 8 };
        let mut entity_resolution_limit: usize = if simple_lookup_like { 4 } else { 6 };

        if intent.aggregate_like {
            root_limit += 1;
            field_limit += 2;
            entity_resolution_limit += 1;
        }
        if intent.compare_like {
            root_limit += 1;
            field_limit += 2;
            entity_resolution_limit += 1;
        }
        if intent.rank_like {
            root_limit += 1;
            field_limit += 1;
        }
        if intent.trend_like {
            root_limit += 2;
            field_limit += 3;
        } else if intent.time_like {
            field_limit += 1;
        }
        if token_count >= 10 {
            root_limit += 1;
            field_limit += 1;
            entity_resolution_limit += 1;
        }
        if quoted_span_count > 0 {
            entity_resolution_limit += 1;
        }
        if identifier_like_token_count > 1 {
            entity_resolution_limit += 1;
        }

        let retrieval_profile = self.query_root_retrieval_profile(query, 6);
        match retrieval_profile.confidence {
            RetrievalConfidence::High => {
                root_limit = root_limit.saturating_sub(1);
                field_limit = field_limit.saturating_sub(1);
                entity_resolution_limit = entity_resolution_limit.saturating_sub(1);
            }
            RetrievalConfidence::Medium => {}
            RetrievalConfidence::Low => {
                if !simple_lookup_like {
                    root_limit += 1;
                    field_limit += 2;
                    entity_resolution_limit += 1;
                    if retrieval_profile.competitive_root_count >= 4 {
                        root_limit += 1;
                        field_limit += 1;
                    }
                }
            }
        }

        PlannerRetrievalBudget {
            root_limit: root_limit.clamp(3, 7),
            field_limit: field_limit.clamp(6, 12),
            entity_resolution_limit: entity_resolution_limit.clamp(4, 8),
        }
    }

    fn base_root_intent_evidence(&self, query: &str) -> Vec<RootIntentEvidence> {
        let query_lower = query.to_lowercase();
        let tokens = Self::query_tokens(query);
        let token_phrases = Self::query_token_phrases(&tokens);
        let mut evidence = self
            .valid_root_fields
            .iter()
            .filter(|root| root.starts_with("query"))
            .map(|root| {
                let root_lower = root.to_lowercase();
                let root_key = root_lower.strip_prefix("query").unwrap_or(&root_lower);
                let return_type = self
                    .query_return_type(root)
                    .unwrap_or_default()
                    .to_lowercase();
                let filter_fields = self
                    .query_filter_input(root)
                    .and_then(|input| self.input_field_names(input))
                    .map(|fields| fields.iter().map(|f| f.to_lowercase()).collect::<Vec<_>>())
                    .unwrap_or_default();
                let scalar_fields = self
                    .root_scalar_fields(root)
                    .into_iter()
                    .map(|field| field.to_lowercase())
                    .collect::<Vec<_>>();
                let relation_fields = self
                    .relation_fields_for_root(root, usize::MAX)
                    .into_iter()
                    .map(|field| field.to_lowercase())
                    .collect::<Vec<_>>();
                let roles = self.field_roles_for_root(root);
                let numeric_fields = roles
                    .numeric_fields
                    .iter()
                    .map(|field| field.to_lowercase())
                    .collect::<Vec<_>>();
                let time_fields = roles
                    .time_fields
                    .iter()
                    .map(|field| field.to_lowercase())
                    .collect::<Vec<_>>();
                let identifier_filter_fields = self
                    .root_identifier_filter_fields(root)
                    .into_iter()
                    .map(|field| field.to_lowercase())
                    .collect::<Vec<_>>();
                let time_filter_fields = self
                    .root_time_filter_fields(root)
                    .into_iter()
                    .map(|field| field.to_lowercase())
                    .collect::<Vec<_>>();
                let entity_field_bonus = self
                    .query_return_type(root)
                    .and_then(|ty| self.object_field_names(ty))
                    .map(|fields| {
                        fields
                            .iter()
                            .filter(|f| {
                                self.domain_config.label_fields.iter().any(|v| v == *f)
                                    || self.domain_config.entity_key_fields.iter().any(|v| v == *f)
                                    || self.domain_config.id_fields.iter().any(|v| v == *f)
                            })
                            .count() as i32
                    })
                    .unwrap_or(0);

                let mut score = 0i32;
                if query_lower.contains(root_key) || query_lower.contains(&return_type) {
                    score += 200;
                }
                for token in &tokens {
                    if root_key.contains(token) {
                        score += 80;
                    }
                    if return_type.contains(token) {
                        score += 60;
                    }
                    if filter_fields
                        .iter()
                        .any(|field| field == token || field.contains(token))
                    {
                        score += 20;
                    }
                    if scalar_fields
                        .iter()
                        .any(|field| field == token || field.contains(token))
                    {
                        score += 25;
                    }
                    if relation_fields
                        .iter()
                        .any(|field| field == token || field.contains(token))
                    {
                        score += 35;
                    }
                }
                for phrase in &token_phrases {
                    if root_key.contains(phrase) {
                        score += 90;
                    }
                    if return_type.contains(phrase) {
                        score += 70;
                    }
                    if filter_fields.iter().any(|field| field.contains(phrase)) {
                        score += 35;
                    }
                    if scalar_fields.iter().any(|field| field.contains(phrase)) {
                        score += 85;
                    }
                    if relation_fields.iter().any(|field| field.contains(phrase)) {
                        score += 50;
                    }
                }
                score += entity_field_bonus * 5;

                let mut time_overlap_candidates = time_fields.clone();
                time_overlap_candidates.extend(time_filter_fields.iter().cloned());

                RootIntentEvidence {
                    root: root.clone(),
                    base_score: score,
                    has_filter_fields: !filter_fields.is_empty(),
                    has_label_fields: !roles.label_fields.is_empty()
                        || !roles.entity_key_fields.is_empty(),
                    has_numeric_fields: !numeric_fields.is_empty(),
                    has_time_fields: !time_fields.is_empty(),
                    has_relation_fields: !relation_fields.is_empty(),
                    has_identifier_filter_fields: !identifier_filter_fields.is_empty(),
                    has_time_filter_fields: !time_filter_fields.is_empty(),
                    numeric_overlap_count: overlap_count(&tokens, &token_phrases, &numeric_fields),
                    time_overlap_count: overlap_count(
                        &tokens,
                        &token_phrases,
                        &time_overlap_candidates,
                    ),
                }
            })
            .filter(|entry| entry.base_score > 0)
            .collect::<Vec<_>>();

        evidence.sort_by(|a, b| {
            b.base_score
                .cmp(&a.base_score)
                .then_with(|| a.root.cmp(&b.root))
        });
        evidence
    }

    fn infer_query_intent(&self, query: &str) -> QueryIntent {
        let evidence = self.base_root_intent_evidence(query);
        self.infer_query_intent_from_evidence(query, &evidence)
    }

    fn infer_query_intent_from_evidence(
        &self,
        query: &str,
        evidence: &[RootIntentEvidence],
    ) -> QueryIntent {
        let top_score = evidence.first().map(|entry| entry.base_score).unwrap_or(0);
        let competitive = evidence
            .iter()
            .filter(|entry| entry.base_score >= (top_score - 35).max(1))
            .take(4)
            .collect::<Vec<_>>();

        let top_window = if competitive.is_empty() {
            evidence.iter().take(4).collect::<Vec<_>>()
        } else {
            competitive
        };

        let metric_phrase = extract_field_phrase_after_by(query);
        let has_metric_field_phrase = metric_phrase.as_ref().is_some_and(|phrase| {
            let needle = phrase.to_ascii_lowercase();
            top_window.iter().any(|entry| {
                self.field_roles_for_root(&entry.root)
                    .numeric_fields
                    .iter()
                    .any(|field| {
                        let lower = field.to_ascii_lowercase();
                        lower == needle || lower.contains(&needle) || needle.contains(&lower)
                    })
            })
        });
        let has_grouping_shape = metric_phrase.is_some()
            && !has_metric_field_phrase
            && top_window
                .iter()
                .any(|entry| entry.has_identifier_filter_fields || entry.has_relation_fields);

        let signals = QueryIntentSignals {
            mention_count: count_query_mentions(query),
            numeric_literal_count: count_numeric_literals(query),
            has_metric_field_phrase,
            has_temporal_literal: has_temporal_literal(query),
            has_explicit_aggregate_operator: false,
            has_grouping_shape,
            top_numeric_root_count: top_window
                .iter()
                .filter(|entry| entry.has_numeric_fields)
                .count(),
            top_time_root_count: top_window
                .iter()
                .filter(|entry| entry.has_time_fields || entry.has_time_filter_fields)
                .count(),
            top_relation_root_count: top_window
                .iter()
                .filter(|entry| entry.has_relation_fields)
                .count(),
            top_identifier_filter_root_count: top_window
                .iter()
                .filter(|entry| entry.has_identifier_filter_fields)
                .count(),
            top_numeric_overlap_count: top_window
                .iter()
                .map(|entry| entry.numeric_overlap_count)
                .max()
                .unwrap_or(0),
            top_time_overlap_count: top_window
                .iter()
                .map(|entry| entry.time_overlap_count)
                .max()
                .unwrap_or(0),
        };

        QueryIntent::infer(&signals)
    }

    fn is_placeholder_scalar(value: &serde_json::Value) -> bool {
        matches!(value, serde_json::Value::String(s) if s.contains("${"))
    }

    fn query_tokens(query: &str) -> Vec<String> {
        let mut out = Vec::new();
        for raw in query
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|token| !token.is_empty())
        {
            let token = raw.to_lowercase();
            if token.len() >= 2 || token.chars().any(|c| c.is_ascii_digit()) {
                if !out.iter().any(|t| t == &token) {
                    out.push(token.clone());
                }
                if token.ends_with('s') && token.len() > 3 {
                    let singular = token[..token.len() - 1].to_string();
                    if !out.iter().any(|t| t == &singular) {
                        out.push(singular);
                    }
                }
            }
        }
        out
    }

    fn query_token_phrases(tokens: &[String]) -> Vec<String> {
        let mut phrases = Vec::new();
        for window in tokens.windows(2) {
            let phrase = format!("{}{}", window[0], window[1]);
            if !phrases.iter().any(|existing| existing == &phrase) {
                phrases.push(phrase);
            }
        }
        phrases
    }

    fn validate_object_selection_set(
        &self,
        type_name: &str,
        selections: &[Selection<'_, String>],
    ) -> PipelineResult<()> {
        let Some(fields) = self.object_fields.get(type_name) else {
            return Ok(());
        };
        let field_types = self.object_field_types.get(type_name);

        for selection in selections {
            match selection {
                Selection::Field(f) => {
                    if !fields.contains(&f.name) {
                        return Err(PipelineError::validation(format!(
                            "Field '{}' does not exist on type '{}'.",
                            f.name, type_name
                        )));
                    }

                    let next_type = field_types.and_then(|m| m.get(&f.name));
                    let has_subselection = !f.selection_set.items.is_empty();
                    let next_is_object =
                        next_type.is_some_and(|t| self.object_fields.contains_key(t));

                    if has_subselection {
                        if next_is_object {
                            if let Some(next_type_name) = next_type {
                                self.validate_object_selection_set(
                                    next_type_name,
                                    &f.selection_set.items,
                                )?;
                            }
                        } else {
                            return Err(PipelineError::validation(format!(
                                "Field '{}' on type '{}' does not support sub-selection.",
                                f.name, type_name
                            )));
                        }
                    } else if next_is_object {
                        return Err(PipelineError::validation(format!(
                            "Field '{}' on type '{}' requires sub-selection.",
                            f.name, type_name
                        )));
                    }
                }
                Selection::InlineFragment(fragment) => {
                    self.validate_object_selection_set(type_name, &fragment.selection_set.items)?;
                }
                Selection::FragmentSpread(_) => {}
            }
        }

        Ok(())
    }

    #[allow(dead_code)]
    pub fn new(schema_str: &str) -> Self {
        Self::with_sls(schema_str, None)
    }

    pub fn with_sls(schema_str: &str, sls: Option<&Sls>) -> Self {
        Self::with_sls_and_source(schema_str, sls, SchemaSource::StaticFile)
    }

    pub fn with_sls_and_source(
        schema_str: &str,
        sls: Option<&Sls>,
        schema_source: SchemaSource,
    ) -> Self {
        let ast = parse_schema::<String>(schema_str).expect("Failed to parse GraphQL schema");
        let mut definitions = HashMap::new();
        let mut valid_root_fields = HashSet::new();
        let mut query_return_types = HashMap::new();
        let mut query_filter_inputs = HashMap::new();
        let mut query_order_inputs = HashMap::new();
        let mut query_arg_type_refs: HashMap<String, HashMap<String, InputTypeRef>> =
            HashMap::new();
        let mut object_fields: HashMap<String, HashSet<String>> = HashMap::new();
        let mut object_field_types: HashMap<String, HashMap<String, String>> = HashMap::new();
        let mut object_field_type_refs: HashMap<String, HashMap<String, InputTypeRef>> =
            HashMap::new();
        let mut input_fields: HashMap<String, HashSet<String>> = HashMap::new();
        let mut input_field_types: HashMap<String, HashMap<String, String>> = HashMap::new();
        let mut input_field_type_refs: HashMap<String, HashMap<String, InputTypeRef>> =
            HashMap::new();
        let mut enum_values_map: HashMap<String, Vec<String>> = HashMap::new();

        for def in ast.definitions {
            if let Definition::TypeDefinition(type_def) = def {
                let (name, def_type) = match &type_def {
                    TypeDefinition::Scalar(t) => (t.name.clone(), DefType::Other),
                    TypeDefinition::Object(t) => {
                        if t.name == "Query" {
                            for field in &t.fields {
                                valid_root_fields.insert(field.name.clone());
                                if let Some(ret) = unwrap_named_type(&field.field_type) {
                                    query_return_types.insert(field.name.clone(), ret);
                                }
                                let mut arg_type_refs = HashMap::new();
                                for arg in &field.arguments {
                                    arg_type_refs.insert(
                                        arg.name.clone(),
                                        type_ref_from_schema_type(&arg.value_type),
                                    );
                                }
                                query_arg_type_refs.insert(field.name.clone(), arg_type_refs);
                                if let Some(filter_ty) = find_arg_type(field, "filter") {
                                    query_filter_inputs.insert(field.name.clone(), filter_ty);
                                }
                                if let Some(order_ty) = find_arg_type(field, "order") {
                                    query_order_inputs.insert(field.name.clone(), order_ty);
                                }
                            }
                        }
                        let mut fields = HashSet::new();
                        let mut field_types = HashMap::new();
                        let mut field_type_refs = HashMap::new();
                        for field in &t.fields {
                            fields.insert(field.name.clone());
                            if let Some(named) = unwrap_named_type(&field.field_type) {
                                field_types.insert(field.name.clone(), named);
                            }
                            field_type_refs.insert(
                                field.name.clone(),
                                type_ref_from_schema_type(&field.field_type),
                            );
                        }
                        object_fields.insert(t.name.clone(), fields);
                        object_field_types.insert(t.name.clone(), field_types);
                        object_field_type_refs.insert(t.name.clone(), field_type_refs);
                        (t.name.clone(), DefType::Object)
                    }
                    TypeDefinition::Interface(t) => (t.name.clone(), DefType::Interface),
                    TypeDefinition::Union(t) => (t.name.clone(), DefType::Other),
                    TypeDefinition::Enum(t) => {
                        let values = t.values.iter().map(|v| v.name.clone()).collect::<Vec<_>>();
                        enum_values_map.insert(t.name.to_lowercase(), values);
                        (t.name.clone(), DefType::Enum)
                    }
                    TypeDefinition::InputObject(t) => {
                        let mut fields = HashSet::new();
                        let mut field_types = HashMap::new();
                        let mut field_type_refs = HashMap::new();
                        for field in &t.fields {
                            fields.insert(field.name.clone());
                            if let Some(named) = unwrap_named_type(&field.value_type) {
                                field_types.insert(field.name.clone(), named);
                            }
                            field_type_refs.insert(
                                field.name.clone(),
                                type_ref_from_schema_type(&field.value_type),
                            );
                        }
                        input_fields.insert(t.name.clone(), fields);
                        input_field_types.insert(t.name.clone(), field_types);
                        input_field_type_refs.insert(t.name.clone(), field_type_refs);
                        (t.name.clone(), DefType::Input)
                    }
                };

                let def_str = format!("{type_def}");
                definitions.insert(
                    name.to_lowercase(),
                    SchemaEntry {
                        def_type,
                        content: def_str,
                    },
                );
            }
        }

        Self::from_parts(
            definitions,
            valid_root_fields,
            query_return_types,
            query_filter_inputs,
            query_order_inputs,
            query_arg_type_refs,
            object_fields,
            object_field_types,
            object_field_type_refs,
            input_fields,
            input_field_types,
            input_field_type_refs,
            enum_values_map,
            sls,
            schema_source,
        )
    }

    pub fn from_introspection_response(
        response: &serde_json::Value,
        sls: Option<&Sls>,
    ) -> PipelineResult<Self> {
        Self::from_introspection_response_with_source(
            response,
            sls,
            SchemaSource::LiveIntrospection,
        )
    }

    pub fn from_introspection_response_with_source(
        response: &serde_json::Value,
        sls: Option<&Sls>,
        schema_source: SchemaSource,
    ) -> PipelineResult<Self> {
        let envelope: IntrospectionEnvelope =
            serde_json::from_value(response.clone()).map_err(|e| {
                PipelineError::validation(format!("Invalid introspection response JSON: {e}"))
            })?;
        let schema = envelope
            .data
            .ok_or_else(|| {
                PipelineError::validation(
                    "Introspection response did not contain a `data` object.".to_string(),
                )
            })?
            .schema;
        let query_root_name = schema
            .query_type
            .and_then(|query_type| query_type.name)
            .ok_or_else(|| {
                PipelineError::validation(
                    "Introspection response did not contain `__schema.queryType.name`.".to_string(),
                )
            })?;

        let mut definitions = HashMap::new();
        let mut valid_root_fields = HashSet::new();
        let mut query_return_types = HashMap::new();
        let mut query_filter_inputs = HashMap::new();
        let mut query_order_inputs = HashMap::new();
        let mut query_arg_type_refs: HashMap<String, HashMap<String, InputTypeRef>> =
            HashMap::new();
        let mut object_fields: HashMap<String, HashSet<String>> = HashMap::new();
        let mut object_field_types: HashMap<String, HashMap<String, String>> = HashMap::new();
        let mut object_field_type_refs: HashMap<String, HashMap<String, InputTypeRef>> =
            HashMap::new();
        let mut input_fields: HashMap<String, HashSet<String>> = HashMap::new();
        let mut input_field_types: HashMap<String, HashMap<String, String>> = HashMap::new();
        let mut input_field_type_refs: HashMap<String, HashMap<String, InputTypeRef>> =
            HashMap::new();
        let mut enum_values_map: HashMap<String, Vec<String>> = HashMap::new();

        for type_def in schema.types {
            let Some(name) = type_def.name.clone() else {
                continue;
            };
            if name.starts_with("__") {
                continue;
            }

            let (def_type, content) = render_introspection_definition(&type_def);
            match type_def.kind.as_str() {
                "OBJECT" | "INTERFACE" => {
                    let mut fields = HashSet::new();
                    let mut field_types = HashMap::new();
                    let mut field_type_refs = HashMap::new();
                    for field in type_def.fields.as_deref().unwrap_or(&[]) {
                        fields.insert(field.name.clone());
                        if let Some(named) = named_type_name(&field.field_type) {
                            field_types.insert(field.name.clone(), named.clone());
                            if name == query_root_name {
                                valid_root_fields.insert(field.name.clone());
                                query_return_types.insert(field.name.clone(), named);
                            }
                        }
                        if name == query_root_name {
                            let mut arg_type_refs = HashMap::new();
                            for arg in &field.args {
                                if let Some(type_ref) = type_ref_from_introspection(&arg.value_type)
                                {
                                    arg_type_refs.insert(arg.name.clone(), type_ref);
                                }
                            }
                            query_arg_type_refs.insert(field.name.clone(), arg_type_refs);
                            if let Some(filter_ty) = find_introspection_arg_type(field, "filter") {
                                query_filter_inputs.insert(field.name.clone(), filter_ty);
                            }
                            if let Some(order_ty) = find_introspection_arg_type(field, "order") {
                                query_order_inputs.insert(field.name.clone(), order_ty);
                            }
                        }
                        if let Some(type_ref) = type_ref_from_introspection(&field.field_type) {
                            field_type_refs.insert(field.name.clone(), type_ref);
                        }
                    }
                    object_fields.insert(name.clone(), fields);
                    object_field_types.insert(name.clone(), field_types);
                    object_field_type_refs.insert(name.clone(), field_type_refs);
                }
                "INPUT_OBJECT" => {
                    let mut fields = HashSet::new();
                    let mut field_types = HashMap::new();
                    let mut field_type_refs = HashMap::new();
                    for field in type_def.input_fields.as_deref().unwrap_or(&[]) {
                        fields.insert(field.name.clone());
                        if let Some(named) = named_type_name(&field.value_type) {
                            field_types.insert(field.name.clone(), named);
                        }
                        if let Some(type_ref) = type_ref_from_introspection(&field.value_type) {
                            field_type_refs.insert(field.name.clone(), type_ref);
                        }
                    }
                    input_fields.insert(name.clone(), fields);
                    input_field_types.insert(name.clone(), field_types);
                    input_field_type_refs.insert(name.clone(), field_type_refs);
                }
                "ENUM" => {
                    let values = type_def
                        .enum_values
                        .as_deref()
                        .unwrap_or(&[])
                        .iter()
                        .map(|value| value.name.clone())
                        .collect::<Vec<_>>();
                    enum_values_map.insert(name.to_lowercase(), values);
                }
                _ => {}
            }

            definitions.insert(name.to_lowercase(), SchemaEntry { def_type, content });
        }

        Ok(Self::from_parts(
            definitions,
            valid_root_fields,
            query_return_types,
            query_filter_inputs,
            query_order_inputs,
            query_arg_type_refs,
            object_fields,
            object_field_types,
            object_field_type_refs,
            input_fields,
            input_field_types,
            input_field_type_refs,
            enum_values_map,
            sls,
            schema_source,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn from_parts(
        definitions: HashMap<String, SchemaEntry>,
        valid_root_fields: HashSet<String>,
        query_return_types: HashMap<String, String>,
        query_filter_inputs: HashMap<String, String>,
        query_order_inputs: HashMap<String, String>,
        query_arg_type_refs: HashMap<String, HashMap<String, InputTypeRef>>,
        object_fields: HashMap<String, HashSet<String>>,
        object_field_types: HashMap<String, HashMap<String, String>>,
        object_field_type_refs: HashMap<String, HashMap<String, InputTypeRef>>,
        input_fields: HashMap<String, HashSet<String>>,
        input_field_types: HashMap<String, HashMap<String, String>>,
        input_field_type_refs: HashMap<String, HashMap<String, InputTypeRef>>,
        enum_values_map: HashMap<String, Vec<String>>,
        sls: Option<&Sls>,
        schema_source: SchemaSource,
    ) -> Self {
        info!(
            "Loaded {} schema definitions and {} root query fields",
            definitions.len(),
            valid_root_fields.len()
        );

        let query_arg_types = query_arg_type_refs
            .iter()
            .map(|(root, args)| {
                (
                    root.clone(),
                    args.iter()
                        .map(|(arg, type_ref)| (arg.clone(), type_ref.name.clone()))
                        .collect::<HashMap<_, _>>(),
                )
            })
            .collect::<HashMap<_, _>>();

        let domain_config = build_domain_config(
            &object_field_types,
            &input_field_types,
            &query_filter_inputs,
            &query_return_types,
            &query_arg_types,
            &enum_values_map,
            sls,
        );
        let concept_aliases_by_type = build_concept_aliases_by_type(&query_return_types, sls);
        let explicit_concept_aliases_by_type =
            build_explicit_concept_aliases_by_type(&query_return_types, sls);
        let intent_vocabulary = sls
            .map(|sls| sls.intent_vocabulary.clone())
            .unwrap_or_default();
        info!(
            "Derived domain config: id_fields={}, numeric_fields={}, latitude_fields={}, longitude_fields={}, geo_object_fields={}, enums={}, roots_with_time_filters={}",
            domain_config.id_fields.len(),
            domain_config.numeric_fields.len(),
            domain_config.location_fields.latitude_fields.len(),
            domain_config.location_fields.longitude_fields.len(),
            domain_config.location_fields.geo_object_fields.len(),
            domain_config.enum_values.len(),
            domain_config
                .root_time_filter_fields
                .iter()
                .filter(|(_, v)| !v.is_empty())
                .count()
        );

        Self {
            definitions,
            valid_root_fields,
            query_return_types,
            query_filter_inputs,
            query_order_inputs,
            query_arg_type_refs,
            object_fields,
            object_field_types,
            object_field_type_refs,
            input_fields,
            input_field_type_refs,
            enum_values_map,
            concept_aliases_by_type,
            explicit_concept_aliases_by_type,
            intent_vocabulary,
            domain_config,
            schema_source,
        }
    }

    pub fn validate_query(&self, query: &str) -> PipelineResult<()> {
        let ast = parse_query::<String>(query)
            .map_err(|e| PipelineError::validation(format!("Invalid GraphQL syntax: {e}")))?;

        fn query_value_to_json(value: &QueryValue<'_, String>) -> Option<serde_json::Value> {
            match value {
                QueryValue::Variable(_) => None,
                QueryValue::Null => Some(serde_json::Value::Null),
                QueryValue::Boolean(b) => Some(serde_json::Value::Bool(*b)),
                QueryValue::String(s) => Some(serde_json::Value::String(s.clone())),
                QueryValue::Enum(s) => Some(serde_json::Value::String(s.clone())),
                QueryValue::Int(n) => n.as_i64().map(|v| serde_json::json!(v)),
                QueryValue::Float(n) => {
                    serde_json::Number::from_f64(*n).map(serde_json::Value::Number)
                }
                QueryValue::List(items) => {
                    let mut out = Vec::with_capacity(items.len());
                    for item in items {
                        out.push(query_value_to_json(item)?);
                    }
                    Some(serde_json::Value::Array(out))
                }
                QueryValue::Object(map) => {
                    let mut out = serde_json::Map::new();
                    for (key, value) in map {
                        out.insert(key.clone(), query_value_to_json(value)?);
                    }
                    Some(serde_json::Value::Object(out))
                }
            }
        }

        fn validate_order_shape(
            registry: &SchemaRegistry,
            root_field: &str,
            order_value: &serde_json::Value,
        ) -> PipelineResult<()> {
            let map = order_value.as_object().ok_or_else(|| {
                PipelineError::validation(
                    "order must be an object with a single direction (asc/desc)".to_string(),
                )
            })?;
            if map.is_empty() {
                return Err(PipelineError::validation(
                    "order cannot be empty".to_string(),
                ));
            }
            let Some(order_input) = registry.query_order_input(root_field) else {
                return Err(PipelineError::validation(format!(
                    "Root field '{}' does not support an order argument.",
                    root_field
                )));
            };
            let Some(order_fields) = registry.input_field_names(order_input) else {
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
                    return Err(PipelineError::validation(
                        "order must specify exactly one of asc or desc".to_string(),
                    ));
                }
            }
            Ok(())
        }

        for def in ast.definitions {
            if let QueryDefinition::Operation(OperationDefinition::Query(q)) = def {
                for selection in q.selection_set.items {
                    if let Selection::Field(f) = selection {
                        if !self.valid_root_fields.contains(&f.name) {
                            // Basic fuzzy suggestion
                            let suggestion = self
                                .valid_root_fields
                                .iter()
                                .find(|valid| {
                                    valid.to_lowercase().contains(&f.name.to_lowercase())
                                        || f.name.to_lowercase().contains(&valid.to_lowercase())
                                })
                                .map(|s| format!(" Did you mean '{s}'?"))
                                .unwrap_or_default();

                            return Err(PipelineError::validation(format!(
                                "Field '{}' does not exist on type 'Query'.{}",
                                f.name, suggestion
                            )));
                        }

                        if let Some(root_type) = self.query_return_types.get(&f.name) {
                            for (arg_name, arg_value) in &f.arguments {
                                if arg_name == "filter" {
                                    let filter_input =
                                        self.query_filter_input(&f.name).ok_or_else(|| {
                                            PipelineError::validation(format!(
                                                "Root field '{}' does not support a filter argument.",
                                                f.name
                                            ))
                                        })?;
                                    if let Some(filter_json) = query_value_to_json(arg_value) {
                                        self.validate_json_value_against_type(
                                            filter_input,
                                            &filter_json,
                                            "filter",
                                        )?;
                                    }
                                } else if arg_name == "order" {
                                    let order_input =
                                        self.query_order_input(&f.name).ok_or_else(|| {
                                            PipelineError::validation(format!(
                                                "Root field '{}' does not support an order argument.",
                                                f.name
                                            ))
                                        })?;
                                    if let Some(order_json) = query_value_to_json(arg_value) {
                                        self.validate_json_value_against_type(
                                            order_input,
                                            &order_json,
                                            "order",
                                        )?;
                                        validate_order_shape(self, &f.name, &order_json)?;
                                    }
                                }
                            }
                            self.validate_object_selection_set(root_type, &f.selection_set.items)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn validate_field_path_for_root(
        &self,
        root_field: &str,
        field_path: &str,
    ) -> PipelineResult<()> {
        let mut current_type = self.query_return_type(root_field).ok_or_else(|| {
            PipelineError::validation(format!(
                "Root field '{}' has no known return type.",
                root_field
            ))
        })?;
        let parts = field_path
            .split('.')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>();
        if parts.is_empty() {
            return Err(PipelineError::validation(format!(
                "Field path '{}' is empty.",
                field_path
            )));
        }

        for (idx, part) in parts.iter().enumerate() {
            let is_last = idx + 1 == parts.len();
            let Some(fields) = self.object_field_names(current_type) else {
                return Err(PipelineError::validation(format!(
                    "Type '{}' does not expose selectable object fields for path '{}'.",
                    current_type, field_path
                )));
            };
            if !fields.contains(*part) {
                return Err(PipelineError::validation(format!(
                    "Field '{}' does not exist on type '{}' (path '{}').",
                    part, current_type, field_path
                )));
            }
            let next_type = self.object_field_type(current_type, part).ok_or_else(|| {
                PipelineError::validation(format!(
                    "Field '{}' on type '{}' has no known type metadata (path '{}').",
                    part, current_type, field_path
                ))
            })?;
            let next_is_object = self.object_fields.contains_key(next_type);
            if !is_last && !next_is_object {
                return Err(PipelineError::validation(format!(
                    "Field '{}' on type '{}' does not support nested selection in path '{}'.",
                    part, current_type, field_path
                )));
            }
            if is_last && next_is_object {
                return Err(PipelineError::validation(format!(
                    "Field path '{}' ends on object field '{}' which requires nested subfields.",
                    field_path, part
                )));
            }
            current_type = next_type;
        }

        Ok(())
    }

    fn validate_json_scalar_value(
        &self,
        type_name: &str,
        value: &serde_json::Value,
        path: &str,
    ) -> PipelineResult<()> {
        if value.is_null() {
            return Ok(());
        }
        if Self::is_placeholder_scalar(value) {
            return Ok(());
        }
        let lower = type_name.to_lowercase();
        let valid = match value {
            serde_json::Value::Array(_) => {
                return Err(PipelineError::validation(format!(
                    "Expected scalar for '{}' at '{}', got array",
                    type_name, path
                )));
            }
            serde_json::Value::Bool(_) => lower == "boolean" || lower == "bool",
            serde_json::Value::Number(n) => {
                if type_name == "Int" {
                    n.as_i64().is_some() || n.as_u64().is_some()
                } else if type_name == "Float" {
                    n.as_f64().is_some()
                } else {
                    lower.contains("int")
                        || lower.contains("float")
                        || lower.contains("double")
                        || lower.contains("decimal")
                        || lower.contains("number")
                }
            }
            serde_json::Value::String(_) => {
                type_name == "String"
                    || type_name == "ID"
                    || lower.contains("string")
                    || lower.contains("time")
                    || lower.contains("date")
            }
            _ => false,
        };
        if valid {
            Ok(())
        } else {
            Err(PipelineError::validation(format!(
                "Value at '{}' is incompatible with type '{}': {}",
                path, type_name, value
            )))
        }
    }

    fn validate_json_value_against_type(
        &self,
        type_name: &str,
        value: &serde_json::Value,
        path: &str,
    ) -> PipelineResult<()> {
        if value.is_null() {
            return Ok(());
        }
        if let Some(enum_values) = self.enum_values_map.get(&type_name.to_lowercase()) {
            match value {
                serde_json::Value::String(s) => {
                    if enum_values.iter().any(|v| v == s) {
                        Ok(())
                    } else {
                        Err(PipelineError::validation(format!(
                            "Invalid enum value '{}' at '{}' for type '{}'. Allowed values: {}",
                            s,
                            path,
                            type_name,
                            enum_values.join(", ")
                        )))
                    }
                }
                serde_json::Value::Array(items) => {
                    for (idx, item) in items.iter().enumerate() {
                        self.validate_json_value_against_type(
                            type_name,
                            item,
                            &format!("{path}[{idx}]"),
                        )?;
                    }
                    Ok(())
                }
                _ => Err(PipelineError::validation(format!(
                    "Expected enum value for '{}' at '{}', got {}",
                    type_name, path, value
                ))),
            }
        } else if let Some(fields) = self.input_fields.get(type_name) {
            let map = value.as_object().ok_or_else(|| {
                PipelineError::validation(format!(
                    "Expected object for input type '{}' at '{}', got {}",
                    type_name, path, value
                ))
            })?;
            for (key, child_value) in map {
                if !fields.contains(key) {
                    return Err(PipelineError::validation(format!(
                        "Field '{}' is not defined on input type '{}' at '{}'.",
                        key, type_name, path
                    )));
                }
                if child_value.is_null() {
                    continue;
                }
                let child_type = self.input_field_type_ref(type_name, key).ok_or_else(|| {
                    PipelineError::validation(format!(
                        "Input field '{}.{}' has no known type metadata.",
                        type_name, key
                    ))
                })?;
                if child_type.is_list {
                    if let Some(items) = child_value.as_array() {
                        for (idx, item) in items.iter().enumerate() {
                            self.validate_json_value_against_type(
                                &child_type.name,
                                item,
                                &format!("{path}.{key}[{idx}]"),
                            )?;
                        }
                    } else {
                        // GraphQL allows coercing single values into lists.
                        self.validate_json_value_against_type(
                            &child_type.name,
                            child_value,
                            &format!("{path}.{key}[0]"),
                        )?;
                    }
                } else {
                    self.validate_json_value_against_type(
                        &child_type.name,
                        child_value,
                        &format!("{path}.{key}"),
                    )?;
                }
            }
            Ok(())
        } else {
            self.validate_json_scalar_value(type_name, value, path)
        }
    }

    pub fn validate_fetch_step(
        &self,
        root_field: &str,
        fields: &[String],
        filter: Option<&serde_json::Value>,
        order: Option<&serde_json::Value>,
    ) -> PipelineResult<()> {
        if !self.valid_root_fields.contains(root_field) {
            return Err(PipelineError::validation(format!(
                "Invalid root_field '{}'.",
                root_field
            )));
        }
        for field_path in fields {
            self.validate_field_path_for_root(root_field, field_path)?;
        }
        if let Some(filter_value) = filter {
            let filter_input = self.query_filter_input(root_field).ok_or_else(|| {
                PipelineError::validation(format!(
                    "Root field '{}' does not support a filter argument.",
                    root_field
                ))
            })?;
            self.validate_json_value_against_type(filter_input, filter_value, "filter")?;
        }
        if let Some(order_value) = order {
            let order_input = self.query_order_input(root_field).ok_or_else(|| {
                PipelineError::validation(format!(
                    "Root field '{}' does not support an order argument.",
                    root_field
                ))
            })?;
            self.validate_json_value_against_type(order_input, order_value, "order")?;
        }
        Ok(())
    }

    pub fn search(&self, query: &str) -> String {
        let query_lower = query.to_lowercase();
        let mut keywords = Self::query_tokens(&query_lower);
        keywords.sort();
        keywords.dedup();

        let mut results: Vec<(i32, String)> = Vec::new();

        for (name, entry) in &self.definitions {
            let mut score = 0;

            // Base score by type priority
            match entry.def_type {
                DefType::Object => score += 50,
                DefType::Interface => score += 40,
                DefType::Enum => score += 20,
                DefType::Input => score += 10,
                DefType::Other => score += 0,
            }

            // Exact match on name (Highest Priority)
            if name == &query_lower {
                score += 500;
            }
            // Contains name
            else if name.contains(&query_lower) {
                score += 200;
            }

            let content_lower = entry.content.to_lowercase();

            // Keyword matching
            for keyword in &keywords {
                if name.contains(keyword) {
                    score += 50;
                }
                if content_lower.contains(keyword) {
                    score += 5; // Content match is weaker than name match
                }
            }

            if score > 10 {
                // Threshold to filter noise
                results.push((score, entry.content.clone()));
            }
        }

        // Sort by score descending
        results.sort_by_key(|entry| std::cmp::Reverse(entry.0));

        // Take top 15 to ensure we capture related inputs/enums if they score well enough
        let top_results: Vec<String> = results.iter().take(15).map(|r| r.1.clone()).collect();

        if top_results.is_empty() {
            return "No matching types found in schema.".to_string();
        }

        top_results.join("\n\n")
    }

    pub fn root_fields(&self) -> Vec<String> {
        let mut fields: Vec<String> = self.valid_root_fields.iter().cloned().collect();
        fields.sort();
        fields
    }

    fn root_scalar_fields(&self, root_field: &str) -> Vec<String> {
        let Some(type_name) = self.query_return_type(root_field) else {
            return Vec::new();
        };
        let Some(field_types) = self.object_field_types.get(type_name) else {
            return Vec::new();
        };
        let mut out = field_types
            .iter()
            .filter_map(|(field, field_type)| {
                if self.object_fields.contains_key(field_type) {
                    None
                } else {
                    Some(field.clone())
                }
            })
            .collect::<Vec<_>>();
        out.sort();
        out
    }

    fn scalar_role_fields_for_root(
        &self,
        root_field: &str,
        candidates: &[String],
        limit: usize,
    ) -> Vec<String> {
        let scalar_fields = self.root_scalar_fields(root_field);
        let preferred_filter_fields = self
            .root_identifier_filter_fields(root_field)
            .into_iter()
            .collect::<HashSet<_>>();
        let mut out = Vec::new();
        for candidate in partition_candidates_by_membership(candidates, &preferred_filter_fields) {
            if scalar_fields.iter().any(|field| field == &candidate) {
                push_unique_string(&mut out, candidate);
            }
            if out.len() >= limit.max(1) {
                break;
            }
        }
        out
    }

    fn relation_fields_for_root(&self, root_field: &str, limit: usize) -> Vec<String> {
        let Some(type_name) = self.query_return_type(root_field) else {
            return Vec::new();
        };
        let Some(field_types) = self.object_field_types.get(type_name) else {
            return Vec::new();
        };
        let mut out = field_types
            .iter()
            .filter_map(|(field, field_type)| {
                if self.object_fields.contains_key(field_type) {
                    Some(field.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        out.sort();
        out.truncate(limit.max(1));
        out
    }

    pub fn field_roles_for_type(&self, type_name: &str) -> FieldRoleSet {
        if let Some(roles) = self.domain_config.type_field_roles.get(type_name) {
            return roles.clone();
        }
        FieldRoleSet {
            id_fields: self.domain_config.id_fields.clone(),
            entity_key_fields: self.domain_config.entity_key_fields.clone(),
            label_fields: self.domain_config.label_fields.clone(),
            numeric_fields: self.domain_config.numeric_fields.clone(),
            time_fields: self.domain_config.time_fields.clone(),
            latitude_fields: self.domain_config.location_fields.latitude_fields.clone(),
            longitude_fields: self.domain_config.location_fields.longitude_fields.clone(),
            geo_object_fields: self.domain_config.location_fields.geo_object_fields.clone(),
        }
    }

    fn scalar_role_fields_for_type(
        &self,
        type_name: &str,
        candidates: &[String],
        limit: usize,
    ) -> Vec<String> {
        let Some(field_types) = self.object_field_types.get(type_name) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for candidate in ordered_unique_candidates(candidates) {
            if field_types
                .get(&candidate)
                .is_some_and(|field_type| !self.object_fields.contains_key(field_type))
            {
                push_unique_string(&mut out, candidate);
            }
            if out.len() >= limit.max(1) {
                break;
            }
        }
        out
    }

    fn default_scalar_fields_for_type(&self, type_name: &str, limit: usize) -> Vec<String> {
        let Some(field_types) = self.object_field_types.get(type_name) else {
            return Vec::new();
        };
        let roles = self.field_roles_for_type(type_name);
        let scalar_fields = field_types
            .iter()
            .filter(|(_, field_type)| !self.object_fields.contains_key(*field_type))
            .map(|(field, _)| field.clone())
            .collect::<Vec<_>>();
        let scalar_field_set = scalar_fields.iter().cloned().collect::<HashSet<_>>();
        let id_field_set = roles.id_fields.iter().cloned().collect::<HashSet<_>>();
        let mut out = Vec::new();

        for group in [
            ordered_unique_candidates(&roles.label_fields),
            ordered_unique_candidates(&roles.time_fields),
            ordered_unique_candidates(&roles.numeric_fields),
            ordered_unique_candidates(&roles.entity_key_fields),
        ] {
            for field in group {
                if scalar_field_set.contains(&field) {
                    push_unique_string(&mut out, field);
                }
                if out.len() >= limit.max(1) {
                    return out;
                }
            }
        }

        let deferred_role_fields = ordered_unique_candidates(&roles.id_fields);
        let deferred_role_field_set = deferred_role_fields.iter().cloned().collect::<HashSet<_>>();
        let mut remaining = scalar_fields
            .into_iter()
            .filter(|field| {
                !out.iter().any(|existing| existing == field)
                    && !id_field_set.contains(field)
                    && !deferred_role_field_set.contains(field)
            })
            .collect::<Vec<_>>();
        remaining.sort();
        for field in remaining {
            push_unique_string(&mut out, field);
            if out.len() >= limit.max(1) {
                return out;
            }
        }

        for field in deferred_role_fields {
            if scalar_field_set.contains(&field) {
                push_unique_string(&mut out, field);
            }
            if out.len() >= limit.max(1) {
                break;
            }
        }
        out
    }

    fn relevant_filter_fields_for_root(
        &self,
        root_field: &str,
        query: &str,
        intent: QueryIntent,
        limit: usize,
    ) -> Vec<String> {
        let tokens = Self::query_tokens(query);
        let roles = self.field_roles_for_root(root_field);
        let filter_fields = self.root_filter_fields(root_field);
        let identifier_filter_fields = self.root_identifier_filter_fields(root_field);
        let time_filter_fields = self.root_time_filter_fields(root_field);
        let mut scored = filter_fields
            .iter()
            .map(|field| {
                let mut score = score_candidate_text(&tokens, field);
                if identifier_filter_fields
                    .iter()
                    .any(|candidate| candidate == field)
                {
                    score += 35;
                }
                if time_filter_fields
                    .iter()
                    .any(|candidate| candidate == field)
                {
                    score += if intent.trend_like || intent.time_like {
                        45
                    } else {
                        20
                    };
                }
                if roles
                    .label_fields
                    .iter()
                    .any(|candidate| candidate == field)
                {
                    score += 20;
                }
                if roles
                    .entity_key_fields
                    .iter()
                    .any(|candidate| candidate == field)
                    || roles.id_fields.iter().any(|candidate| candidate == field)
                {
                    score += 25;
                }
                (score, field.clone())
            })
            .collect::<Vec<_>>();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));

        let mut out = Vec::new();
        for (score, field) in scored {
            if score > 0 {
                push_unique_string(&mut out, field);
            }
            if out.len() >= limit.max(1) {
                break;
            }
        }
        if out.is_empty() {
            for field in identifier_filter_fields
                .iter()
                .chain(time_filter_fields.iter())
                .chain(filter_fields.iter())
            {
                push_unique_string(&mut out, field.clone());
                if out.len() >= limit.max(1) {
                    break;
                }
            }
        }
        out
    }

    fn relation_leaf_paths_for_root(
        &self,
        root_field: &str,
        query: &str,
        intent: QueryIntent,
        limit: usize,
    ) -> Vec<String> {
        let tokens = Self::query_tokens(query);
        let Some(type_name) = self.query_return_type(root_field) else {
            return Vec::new();
        };
        let Some(field_types) = self.object_field_types.get(type_name) else {
            return Vec::new();
        };

        let mut scored = Vec::new();
        for (relation_field, relation_type) in field_types {
            if !self.object_fields.contains_key(relation_type) {
                continue;
            }
            let relation_query_relevance = score_candidate_text(&tokens, relation_field)
                + score_candidate_text(&tokens, relation_type);
            if relation_query_relevance < MIN_RELATION_QUERY_RELEVANCE {
                continue;
            }
            let relation_roles = self.field_roles_for_type(relation_type);
            let mut leaf_fields = Vec::new();
            let key_fields = self.scalar_role_fields_for_type(
                relation_type,
                &relation_roles
                    .label_fields
                    .iter()
                    .chain(relation_roles.entity_key_fields.iter())
                    .chain(relation_roles.id_fields.iter())
                    .cloned()
                    .collect::<Vec<_>>(),
                limit.max(1),
            );
            let numeric_fields =
                self.scalar_role_fields_for_type(relation_type, &relation_roles.numeric_fields, 2);
            let time_fields =
                self.scalar_role_fields_for_type(relation_type, &relation_roles.time_fields, 2);

            for field in &key_fields {
                push_unique_string(&mut leaf_fields, field.clone());
            }
            if intent.aggregate_like || intent.compare_like || intent.rank_like {
                for field in &numeric_fields {
                    push_unique_string(&mut leaf_fields, field.clone());
                }
            }
            if intent.trend_like || intent.time_like {
                for field in &time_fields {
                    push_unique_string(&mut leaf_fields, field.clone());
                }
            }
            if leaf_fields.is_empty() {
                for field in self.default_scalar_fields_for_type(relation_type, 2) {
                    push_unique_string(&mut leaf_fields, field);
                }
            }

            for leaf in leaf_fields {
                let path = format!("{relation_field}.{leaf}");
                let mut score = relation_query_relevance * 2 + score_candidate_text(&tokens, &leaf);
                if key_fields.iter().any(|candidate| candidate == &leaf) {
                    score += 20;
                }
                if numeric_fields.iter().any(|candidate| candidate == &leaf)
                    && (intent.aggregate_like || intent.compare_like || intent.rank_like)
                {
                    score += 20;
                }
                if time_fields.iter().any(|candidate| candidate == &leaf)
                    && (intent.trend_like || intent.time_like)
                {
                    score += 20;
                }
                scored.push((score, path));
            }
        }

        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let mut out = Vec::new();
        for (score, path) in scored {
            if score > 0 {
                push_unique_string(&mut out, path);
            }
            if out.len() >= limit.max(1) {
                break;
            }
        }
        out
    }

    fn structural_relation_leaf_paths_for_root(
        &self,
        root_field: &str,
        limit: usize,
    ) -> Vec<String> {
        let Some(type_name) = self.query_return_type(root_field) else {
            return Vec::new();
        };
        let Some(field_types) = self.object_field_types.get(type_name) else {
            return Vec::new();
        };

        let mut relation_fields = field_types
            .iter()
            .filter_map(|(field, relation_type)| {
                let is_collection_relation = self
                    .object_field_type_refs
                    .get(type_name)
                    .and_then(|type_refs| type_refs.get(field))
                    .is_some_and(|type_ref| type_ref.is_list);
                if !is_collection_relation {
                    return None;
                }
                self.object_fields
                    .contains_key(relation_type)
                    .then_some((field.clone(), relation_type.clone()))
            })
            .collect::<Vec<_>>();
        relation_fields.sort_by(|a, b| a.0.cmp(&b.0));

        let mut relation_leaf_fields = Vec::new();
        for (relation_field, relation_type) in relation_fields {
            let relation_roles = self.field_roles_for_type(&relation_type);
            let key_fields = self.scalar_role_fields_for_type(
                &relation_type,
                &relation_roles
                    .label_fields
                    .iter()
                    .chain(relation_roles.entity_key_fields.iter())
                    .chain(relation_roles.id_fields.iter())
                    .cloned()
                    .collect::<Vec<_>>(),
                2,
            );
            let numeric_fields =
                self.scalar_role_fields_for_type(&relation_type, &relation_roles.numeric_fields, 1);
            let time_fields =
                self.scalar_role_fields_for_type(&relation_type, &relation_roles.time_fields, 1);

            let mut leaf_fields = Vec::new();
            for field in key_fields
                .iter()
                .chain(numeric_fields.iter())
                .chain(time_fields.iter())
            {
                push_unique_string(&mut leaf_fields, field.clone());
            }
            if leaf_fields.is_empty() {
                for field in self.default_scalar_fields_for_type(&relation_type, 2) {
                    push_unique_string(&mut leaf_fields, field);
                }
            }
            relation_leaf_fields.push((relation_field, leaf_fields));
        }

        let max_leaf_count = relation_leaf_fields
            .iter()
            .map(|(_, leaf_fields)| leaf_fields.len())
            .max()
            .unwrap_or(0);
        let mut out = Vec::new();
        for leaf_index in 0..max_leaf_count {
            for (relation_field, leaf_fields) in &relation_leaf_fields {
                let Some(leaf) = leaf_fields.get(leaf_index) else {
                    continue;
                };
                push_unique_string(&mut out, format!("{relation_field}.{leaf}"));
                if out.len() >= limit.max(1) {
                    return out;
                }
            }
        }
        out
    }

    fn intent_fields_for_root(
        &self,
        root_field: &str,
        intent: QueryIntent,
        limit: usize,
    ) -> Vec<String> {
        let roles = self.field_roles_for_root(root_field);
        let mut out = Vec::new();

        let key_fields = self.scalar_role_fields_for_root(
            root_field,
            &roles
                .label_fields
                .iter()
                .chain(roles.entity_key_fields.iter())
                .chain(roles.id_fields.iter())
                .cloned()
                .collect::<Vec<_>>(),
            limit.max(1),
        );
        let numeric_fields =
            self.scalar_role_fields_for_root(root_field, &roles.numeric_fields, limit.max(1));
        let time_fields =
            self.scalar_role_fields_for_root(root_field, &roles.time_fields, limit.max(1));

        if intent.trend_like {
            for field in &time_fields {
                push_unique_string(&mut out, field.clone());
            }
            for field in &numeric_fields {
                push_unique_string(&mut out, field.clone());
            }
            for field in &key_fields {
                push_unique_string(&mut out, field.clone());
            }
        } else if intent.compare_like || intent.aggregate_like || intent.rank_like {
            for field in &key_fields {
                push_unique_string(&mut out, field.clone());
            }
            for field in &numeric_fields {
                push_unique_string(&mut out, field.clone());
            }
            if intent.time_like {
                for field in &time_fields {
                    push_unique_string(&mut out, field.clone());
                }
            }
        }

        if out.is_empty() {
            out = self.default_scalar_fields_for_root(root_field, limit.max(1));
        } else {
            out.truncate(limit.max(1));
        }

        out
    }

    fn scored_query_roots(&self, query: &str) -> Vec<QueryRootMatch> {
        let base_evidence = self.base_root_intent_evidence(query);
        let intent = self.infer_query_intent_from_evidence(query, &base_evidence);
        let mut scored = base_evidence
            .into_iter()
            .map(|entry| {
                let mut score = entry.base_score;

                if intent.aggregate_like {
                    if entry.has_label_fields {
                        score += 35;
                    }
                    if entry.has_filter_fields {
                        score += 15;
                    }
                    if entry.has_numeric_fields {
                        score += 25;
                    }
                }
                if intent.compare_like {
                    if entry.has_numeric_fields {
                        score += 45;
                    }
                    if entry.has_identifier_filter_fields {
                        score += 20;
                    }
                    if entry.has_label_fields {
                        score += 20;
                    }
                }
                if intent.rank_like {
                    if entry.has_numeric_fields {
                        score += 35;
                    }
                    if entry.has_label_fields {
                        score += 15;
                    }
                }
                if intent.trend_like {
                    if entry.has_time_fields {
                        score += 50;
                    }
                    if entry.has_time_filter_fields {
                        score += 40;
                    }
                    if entry.has_numeric_fields {
                        score += 30;
                    }
                } else if intent.time_like {
                    if entry.has_time_fields {
                        score += 20;
                    }
                    if entry.has_time_filter_fields {
                        score += 20;
                    }
                }

                QueryRootMatch {
                    root: entry.root,
                    score,
                }
            })
            .collect::<Vec<_>>();

        scored.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.root.cmp(&b.root)));
        scored
    }

    pub fn best_matching_query_roots_scored(
        &self,
        query: &str,
        limit: usize,
    ) -> Vec<QueryRootMatch> {
        self.scored_query_roots(query)
            .into_iter()
            .take(limit.max(1))
            .collect()
    }

    pub fn query_root_retrieval_profile(
        &self,
        query: &str,
        limit: usize,
    ) -> QueryRootRetrievalProfile {
        let matches = self.best_matching_query_roots_scored(query, limit.max(1));
        let top_score = matches.first().map(|entry| entry.score).unwrap_or(0);
        let runner_up_score = matches.get(1).map(|entry| entry.score).unwrap_or(0);
        let competitive_root_count = if top_score == 0 {
            0
        } else {
            matches
                .iter()
                .filter(|entry| entry.score >= (top_score - 25).max(1))
                .count()
        };
        let confidence =
            if top_score >= 160 && top_score - runner_up_score >= 25 && competitive_root_count <= 2
            {
                RetrievalConfidence::High
            } else if top_score == 0
                || top_score < 140
                || (top_score - runner_up_score <= 20 && competitive_root_count >= 2)
                || competitive_root_count >= 4
            {
                RetrievalConfidence::Low
            } else {
                RetrievalConfidence::Medium
            };

        QueryRootRetrievalProfile {
            matches,
            top_score,
            runner_up_score,
            competitive_root_count,
            confidence,
        }
    }

    pub fn best_matching_query_roots(&self, query: &str, limit: usize) -> Vec<String> {
        self.scored_query_roots(query)
            .into_iter()
            .take(limit.max(1))
            .map(|entry| entry.root)
            .collect()
    }

    pub fn schema_retrieval_slice(
        &self,
        query: &str,
        root_limit: usize,
        field_limit: usize,
    ) -> SchemaRetrievalSlice {
        let intent = self.infer_query_intent(query);
        let profile = self.query_root_retrieval_profile(query, root_limit.max(1));
        let field_limit = field_limit.max(1);
        let mut roots = Vec::new();

        for root_match in &profile.matches {
            let root = root_match.root.clone();
            let return_type = self.query_return_type(&root).unwrap_or("unknown");
            let default_scalar_fields = self.default_scalar_fields_for_root(&root, field_limit);
            let roles = self.field_roles_for_root(&root);
            let numeric_fields =
                self.scalar_role_fields_for_root(&root, &roles.numeric_fields, field_limit);
            let time_fields =
                self.scalar_role_fields_for_root(&root, &roles.time_fields, field_limit);
            let relation_fields =
                self.relation_leaf_paths_for_root(&root, query, intent, field_limit);
            let filter_fields =
                self.relevant_filter_fields_for_root(&root, query, intent, field_limit);
            let identifier_filter_fields = self
                .root_identifier_filter_fields(&root)
                .into_iter()
                .take(field_limit)
                .collect::<Vec<_>>();
            let time_filter_fields = self
                .root_time_filter_fields(&root)
                .into_iter()
                .take(field_limit)
                .collect::<Vec<_>>();
            let intent_fields = self.intent_fields_for_root(&root, intent, field_limit);
            let concept_aliases = self.concept_aliases_for_type(return_type);

            let mut key_fields = Vec::new();
            for field in roles
                .label_fields
                .iter()
                .chain(roles.entity_key_fields.iter())
                .chain(roles.id_fields.iter())
            {
                if !key_fields.iter().any(|existing: &String| existing == field)
                    && default_scalar_fields
                        .iter()
                        .any(|candidate| candidate == field)
                {
                    key_fields.push(field.clone());
                }
            }
            if key_fields.is_empty() {
                key_fields.extend(default_scalar_fields.iter().take(3).cloned());
            }

            roots.push(RetrievedRootSlice {
                root,
                score: root_match.score,
                capability_evidence: Vec::new(),
                return_type: return_type.to_string(),
                concept_aliases,
                key_fields,
                intent_fields,
                default_scalar_fields,
                numeric_fields,
                time_fields,
                relation_fields,
                filter_fields,
                identifier_filter_fields,
                time_filter_fields,
            });
        }

        SchemaRetrievalSlice {
            intent: intent.describe(),
            profile,
            roots,
        }
    }

    #[cfg(test)]
    pub fn planner_context(&self, query: &str, root_limit: usize, field_limit: usize) -> String {
        let slice = self.schema_retrieval_slice(query, root_limit, field_limit);
        if slice.roots.is_empty() {
            return self.search(query);
        }

        self.planner_context_from_slice(&slice)
    }

    pub fn planner_context_from_slice(&self, slice: &SchemaRetrievalSlice) -> String {
        let mut lines = Vec::new();
        lines.push(format!(
            "Likely query roots and usable fields (intent: {}, retrieval_confidence: {}, competitive_roots: {}):",
            slice.intent,
            slice.profile.confidence.as_str(),
            slice.profile.competitive_root_count
        ));

        for root_slice in &slice.roots {
            let filter_preview = if root_slice.filter_fields.is_empty() {
                "(none)".to_string()
            } else {
                root_slice.filter_fields.join(", ")
            };
            let field_preview = if root_slice.default_scalar_fields.is_empty() {
                "(none)".to_string()
            } else {
                root_slice.default_scalar_fields.join(", ")
            };
            let intent_field_preview = if root_slice.intent_fields.is_empty() {
                "(none)".to_string()
            } else {
                root_slice.intent_fields.join(", ")
            };
            let key_preview = if root_slice.key_fields.is_empty() {
                "(none)".to_string()
            } else {
                root_slice.key_fields.join(", ")
            };
            let numeric_preview = if root_slice.numeric_fields.is_empty() {
                "(none)".to_string()
            } else {
                root_slice.numeric_fields.join(", ")
            };
            let time_preview = if root_slice.time_fields.is_empty() {
                "(none)".to_string()
            } else {
                root_slice.time_fields.join(", ")
            };
            let relation_preview = if root_slice.relation_fields.is_empty() {
                "(none)".to_string()
            } else {
                root_slice.relation_fields.join(", ")
            };
            let identifier_filter_preview = if root_slice.identifier_filter_fields.is_empty() {
                "(none)".to_string()
            } else {
                root_slice.identifier_filter_fields.join(", ")
            };
            let time_filter_preview = if root_slice.time_filter_fields.is_empty() {
                "(none)".to_string()
            } else {
                root_slice.time_filter_fields.join(", ")
            };

            lines.push(format!(
                "- root: {} (score: {})",
                root_slice.root, root_slice.score
            ));
            if !root_slice.capability_evidence.is_empty() {
                lines.push(format!(
                    "  capability_evidence: {}",
                    root_slice.capability_evidence.join("; ")
                ));
            }
            lines.push(format!("  return_type: {}", root_slice.return_type));
            lines.push(format!("  key_fields: {key_preview}"));
            lines.push(format!("  intent_fields: {intent_field_preview}"));
            lines.push(format!("  default_scalar_fields: {field_preview}"));
            lines.push(format!("  numeric_fields: {numeric_preview}"));
            lines.push(format!("  time_fields: {time_preview}"));
            lines.push(format!("  relation_fields: {relation_preview}"));
            lines.push(format!("  filter_fields: {filter_preview}"));
            lines.push(format!(
                "  identifier_filter_fields: {identifier_filter_preview}"
            ));
            lines.push(format!("  time_filter_fields: {time_filter_preview}"));
        }

        lines.join("\n")
    }

    pub fn query_roots_for_type(&self, type_name: &str) -> Vec<String> {
        let mut out = self
            .query_return_types
            .iter()
            .filter_map(|(root, return_type)| {
                (root.starts_with("query") && return_type.eq_ignore_ascii_case(type_name))
                    .then_some(root.clone())
            })
            .collect::<Vec<_>>();
        out.sort();
        out.dedup();
        out
    }

    pub fn relation_neighbor_query_roots(&self, root_field: &str) -> Vec<String> {
        let Some(type_name) = self.query_return_type(root_field) else {
            return Vec::new();
        };
        let Some(field_types) = self.object_field_types.get(type_name) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut relations = field_types
            .iter()
            .filter_map(|(field, relation_type)| {
                let is_collection_relation = self
                    .object_field_type_refs
                    .get(type_name)
                    .and_then(|type_refs| type_refs.get(field))
                    .is_some_and(|type_ref| type_ref.is_list);
                if !is_collection_relation {
                    return None;
                }
                self.object_fields
                    .contains_key(relation_type)
                    .then_some(relation_type.clone())
            })
            .collect::<Vec<_>>();
        relations.sort();
        relations.dedup();
        for relation_type in relations {
            for root in self.query_roots_for_type(&relation_type) {
                push_unique_string(&mut out, root);
            }
        }
        out
    }

    pub fn anchored_planner_context(
        &self,
        query: &str,
        roots: &[String],
        field_limit: usize,
    ) -> String {
        let intent = self.infer_query_intent(query);
        let mut anchored_roots = Vec::new();
        for root in roots {
            if root.starts_with("query") && self.query_return_type(root).is_some() {
                push_unique_string(&mut anchored_roots, root.clone());
            }
        }
        if anchored_roots.is_empty() {
            return self.search(query);
        }

        let mut lines = Vec::new();
        lines.push(format!(
            "Entity-anchored schema slice (intent: {}):",
            intent.describe()
        ));

        for root in anchored_roots {
            let return_type = self.query_return_type(&root).unwrap_or("unknown");
            let aliases = self.concept_aliases_for_type(return_type);
            let default_fields = self.default_scalar_fields_for_root(&root, field_limit.max(1));
            let roles = self.field_roles_for_root(&root);
            let numeric_fields =
                self.scalar_role_fields_for_root(&root, &roles.numeric_fields, field_limit.max(1));
            let time_fields =
                self.scalar_role_fields_for_root(&root, &roles.time_fields, field_limit.max(1));
            let relation_fields =
                self.structural_relation_leaf_paths_for_root(&root, field_limit.max(1));
            let filter_fields = self.root_filter_fields(&root);
            let identifier_filter_fields = self
                .root_identifier_filter_fields(&root)
                .into_iter()
                .take(field_limit.max(1))
                .collect::<Vec<_>>();
            let time_filter_fields = self
                .root_time_filter_fields(&root)
                .into_iter()
                .take(field_limit.max(1))
                .collect::<Vec<_>>();
            let key_fields = self.scalar_role_fields_for_root(
                &root,
                &roles
                    .label_fields
                    .iter()
                    .chain(roles.entity_key_fields.iter())
                    .chain(roles.id_fields.iter())
                    .cloned()
                    .collect::<Vec<_>>(),
                field_limit.max(1),
            );

            let alias_preview = if aliases.is_empty() {
                "(none)".to_string()
            } else {
                aliases.join(", ")
            };
            let key_preview = if key_fields.is_empty() {
                "(none)".to_string()
            } else {
                key_fields.join(", ")
            };
            let field_preview = if default_fields.is_empty() {
                "(none)".to_string()
            } else {
                default_fields.join(", ")
            };
            let numeric_preview = if numeric_fields.is_empty() {
                "(none)".to_string()
            } else {
                numeric_fields.join(", ")
            };
            let time_preview = if time_fields.is_empty() {
                "(none)".to_string()
            } else {
                time_fields.join(", ")
            };
            let relation_preview = if relation_fields.is_empty() {
                "(none)".to_string()
            } else {
                relation_fields.join(", ")
            };
            let filter_preview = if filter_fields.is_empty() {
                "(none)".to_string()
            } else {
                filter_fields
                    .into_iter()
                    .take(field_limit.max(1))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let identifier_filter_preview = if identifier_filter_fields.is_empty() {
                "(none)".to_string()
            } else {
                identifier_filter_fields.join(", ")
            };
            let time_filter_preview = if time_filter_fields.is_empty() {
                "(none)".to_string()
            } else {
                time_filter_fields.join(", ")
            };

            lines.push(format!("- root: {root}"));
            lines.push(format!("  return_type: {return_type}"));
            lines.push(format!("  concept_aliases: {alias_preview}"));
            lines.push(format!("  key_fields: {key_preview}"));
            lines.push(format!("  default_scalar_fields: {field_preview}"));
            lines.push(format!("  numeric_fields: {numeric_preview}"));
            lines.push(format!("  time_fields: {time_preview}"));
            lines.push(format!("  relation_fields: {relation_preview}"));
            lines.push(format!("  filter_fields: {filter_preview}"));
            lines.push(format!(
                "  identifier_filter_fields: {identifier_filter_preview}"
            ));
            lines.push(format!("  time_filter_fields: {time_filter_preview}"));
        }

        lines.join("\n")
    }

    pub fn query_return_type(&self, root_field: &str) -> Option<&str> {
        self.query_return_types.get(root_field).map(String::as_str)
    }

    pub fn query_filter_input(&self, root_field: &str) -> Option<&str> {
        self.query_filter_inputs.get(root_field).map(String::as_str)
    }

    pub fn query_order_input(&self, root_field: &str) -> Option<&str> {
        self.query_order_inputs.get(root_field).map(String::as_str)
    }

    pub fn query_root_fields(&self) -> Vec<String> {
        let mut out = self.valid_root_fields.iter().cloned().collect::<Vec<_>>();
        out.sort();
        out
    }

    pub fn root_arg_names(&self, root_field: &str) -> Vec<String> {
        let Some(fields) = self.query_arg_type_refs.get(root_field) else {
            return Vec::new();
        };
        let mut out = fields.keys().cloned().collect::<Vec<_>>();
        out.sort();
        out
    }

    pub fn root_arg_type_ref(&self, root_field: &str, arg_name: &str) -> Option<InputTypeRef> {
        let map = self.query_arg_type_refs.get(root_field)?;
        if let Some(found) = map.get(arg_name) {
            return Some(found.clone());
        }
        map.iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(arg_name))
            .map(|(_, ty)| ty.clone())
    }

    pub fn object_field_names(&self, type_name: &str) -> Option<&HashSet<String>> {
        self.object_fields.get(type_name)
    }

    pub fn object_field_type(&self, type_name: &str, field: &str) -> Option<&str> {
        self.object_field_types
            .get(type_name)
            .and_then(|m| m.get(field))
            .map(String::as_str)
    }

    #[cfg(test)]
    pub fn object_field_type_ref(&self, type_name: &str, field: &str) -> Option<InputTypeRef> {
        self.object_field_type_refs
            .get(type_name)
            .and_then(|m| m.get(field))
            .cloned()
    }

    pub fn object_fields_with_type(&self, parent_type: &str, target_type: &str) -> Vec<String> {
        let Some(field_types) = self.object_field_types.get(parent_type) else {
            return Vec::new();
        };
        let mut out = field_types
            .iter()
            .filter_map(|(field, field_type)| {
                if field_type == target_type {
                    Some(field.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        out.sort();
        out
    }

    pub fn input_field_names(&self, input_name: &str) -> Option<&HashSet<String>> {
        self.input_fields.get(input_name)
    }

    pub fn input_field_type(&self, input_name: &str, field: &str) -> Option<&str> {
        self.input_field_type_refs
            .get(input_name)
            .and_then(|m| m.get(field))
            .map(|t| t.name.as_str())
    }

    pub fn input_field_type_ref(&self, input_name: &str, field: &str) -> Option<&InputTypeRef> {
        self.input_field_type_refs
            .get(input_name)
            .and_then(|m| m.get(field))
    }

    pub fn filter_field_type_ref(&self, root_field: &str, field: &str) -> Option<InputTypeRef> {
        let input_name = self.query_filter_input(root_field)?;
        let map = self.input_field_type_refs.get(input_name)?;
        if let Some(found) = map.get(field) {
            return Some(found.clone());
        }
        map.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(field))
            .map(|(_, v)| v.clone())
    }

    #[allow(dead_code)]
    pub fn enum_values(&self, enum_name: &str) -> Option<Vec<String>> {
        self.enum_values_map.get(&enum_name.to_lowercase()).cloned()
    }

    pub fn domain_config(&self) -> &DomainConfig {
        &self.domain_config
    }

    pub fn field_roles_for_root(&self, root_field: &str) -> FieldRoleSet {
        if let Some(roles) = self.domain_config.root_field_roles.get(root_field) {
            return roles.clone();
        }
        if let Some(return_type) = self.query_return_type(root_field)
            && let Some(roles) = self.domain_config.type_field_roles.get(return_type)
        {
            return roles.clone();
        }
        FieldRoleSet {
            id_fields: self.domain_config.id_fields.clone(),
            entity_key_fields: self.domain_config.entity_key_fields.clone(),
            label_fields: self.domain_config.label_fields.clone(),
            numeric_fields: self.domain_config.numeric_fields.clone(),
            time_fields: self.domain_config.time_fields.clone(),
            latitude_fields: self.domain_config.location_fields.latitude_fields.clone(),
            longitude_fields: self.domain_config.location_fields.longitude_fields.clone(),
            geo_object_fields: self.domain_config.location_fields.geo_object_fields.clone(),
        }
    }

    #[allow(dead_code)]
    pub fn schema_source(&self) -> SchemaSource {
        self.schema_source
    }

    pub fn root_identifier_filter_fields(&self, root_field: &str) -> Vec<String> {
        self.domain_config
            .root_identifier_filter_fields
            .get(root_field)
            .cloned()
            .unwrap_or_default()
    }

    pub fn concept_aliases_for_type(&self, type_name: &str) -> Vec<String> {
        self.concept_aliases_by_type
            .get(&type_name.to_ascii_lowercase())
            .cloned()
            .unwrap_or_else(|| generated_concept_aliases(type_name))
    }

    pub fn explicit_concept_aliases_for_type(&self, type_name: &str) -> Vec<String> {
        self.explicit_concept_aliases_by_type
            .get(&type_name.to_ascii_lowercase())
            .cloned()
            .unwrap_or_default()
    }

    pub fn intent_vocabulary(&self) -> &IntentVocabulary {
        &self.intent_vocabulary
    }

    pub fn root_filter_object_fields(&self, root_field: &str) -> Vec<String> {
        self.domain_config
            .root_filter_object_fields
            .get(root_field)
            .cloned()
            .unwrap_or_default()
    }

    pub fn root_time_filter_fields(&self, root_field: &str) -> Vec<String> {
        self.domain_config
            .root_time_filter_fields
            .get(root_field)
            .cloned()
            .unwrap_or_default()
    }

    pub fn root_filter_fields(&self, root_field: &str) -> Vec<String> {
        let Some(input_name) = self.query_filter_input(root_field) else {
            return Vec::new();
        };
        let Some(fields) = self.input_field_names(input_name) else {
            return Vec::new();
        };
        let mut out = fields.iter().cloned().collect::<Vec<_>>();
        out.sort();
        out
    }

    pub fn default_scalar_fields_for_root(&self, root_field: &str, limit: usize) -> Vec<String> {
        let Some(type_name) = self.query_return_type(root_field) else {
            return Vec::new();
        };
        let Some(field_types) = self.object_field_types.get(type_name) else {
            return Vec::new();
        };
        let roles = self.field_roles_for_root(root_field);
        let scalar_fields = field_types
            .iter()
            .filter(|(_, field_type)| !self.object_fields.contains_key(*field_type))
            .map(|(field, _)| field.clone())
            .collect::<Vec<_>>();
        let scalar_field_set = scalar_fields.iter().cloned().collect::<HashSet<_>>();
        let id_field_set = roles.id_fields.iter().cloned().collect::<HashSet<_>>();
        let type_roles = self.field_roles_for_type(type_name);
        let preferred_filter_fields = self
            .root_identifier_filter_fields(root_field)
            .into_iter()
            .collect::<HashSet<_>>();
        let mut out = Vec::new();

        for group in [
            partition_candidates_by_membership(&roles.label_fields, &preferred_filter_fields),
            ordered_unique_candidates(&roles.time_fields),
            ordered_unique_candidates(&roles.numeric_fields),
            partition_candidates_by_membership(&roles.entity_key_fields, &preferred_filter_fields),
        ] {
            for field in group {
                if scalar_field_set.contains(&field) {
                    push_unique_string(&mut out, field);
                }
                if out.len() >= limit.max(1) {
                    return out;
                }
            }
        }

        let deferred_role_fields = ordered_unique_candidates(
            &type_roles
                .label_fields
                .iter()
                .chain(type_roles.time_fields.iter())
                .chain(type_roles.numeric_fields.iter())
                .chain(type_roles.entity_key_fields.iter())
                .chain(type_roles.id_fields.iter())
                .cloned()
                .collect::<Vec<_>>(),
        );
        let deferred_role_field_set = deferred_role_fields.iter().cloned().collect::<HashSet<_>>();
        let mut remaining = scalar_fields
            .into_iter()
            .filter(|field| {
                !out.iter().any(|existing| existing == field)
                    && !id_field_set.contains(field)
                    && !deferred_role_field_set.contains(field)
            })
            .collect::<Vec<_>>();
        remaining.sort();
        for field in remaining {
            push_unique_string(&mut out, field);
            if out.len() >= limit.max(1) {
                return out;
            }
        }

        for field in deferred_role_fields {
            if scalar_field_set.contains(&field) {
                push_unique_string(&mut out, field);
            }
            if out.len() >= limit.max(1) {
                break;
            }
        }
        out
    }
}

#[derive(Debug, Deserialize)]
struct IntrospectionEnvelope {
    data: Option<IntrospectionData>,
}

#[derive(Debug, Deserialize)]
struct IntrospectionData {
    #[serde(rename = "__schema")]
    schema: IntrospectionSchema,
}

#[derive(Debug, Deserialize)]
struct IntrospectionSchema {
    #[serde(rename = "queryType")]
    query_type: Option<IntrospectionNamedType>,
    types: Vec<IntrospectionTypeDef>,
}

#[derive(Debug, Deserialize)]
struct IntrospectionNamedType {
    name: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct IntrospectionTypeDef {
    kind: String,
    name: Option<String>,
    #[serde(default)]
    fields: Option<Vec<IntrospectionField>>,
    #[serde(default, rename = "inputFields")]
    input_fields: Option<Vec<IntrospectionInputValue>>,
    #[serde(default, rename = "enumValues")]
    enum_values: Option<Vec<IntrospectionEnumValue>>,
}

#[derive(Clone, Debug, Deserialize)]
struct IntrospectionField {
    name: String,
    #[serde(default)]
    args: Vec<IntrospectionInputValue>,
    #[serde(rename = "type")]
    field_type: IntrospectionTypeRef,
}

#[derive(Clone, Debug, Deserialize)]
struct IntrospectionInputValue {
    name: String,
    #[serde(rename = "type")]
    value_type: IntrospectionTypeRef,
}

#[derive(Clone, Debug, Deserialize)]
struct IntrospectionEnumValue {
    name: String,
}

#[derive(Clone, Debug, Deserialize)]
struct IntrospectionTypeRef {
    kind: String,
    name: Option<String>,
    #[serde(rename = "ofType")]
    of_type: Option<Box<IntrospectionTypeRef>>,
}

fn named_type_name(type_ref: &IntrospectionTypeRef) -> Option<String> {
    if let Some(name) = &type_ref.name {
        return Some(name.clone());
    }
    type_ref.of_type.as_deref().and_then(named_type_name)
}

fn render_type_ref(type_ref: &IntrospectionTypeRef) -> String {
    match type_ref.kind.as_str() {
        "NON_NULL" => type_ref
            .of_type
            .as_deref()
            .map(|inner| format!("{}!", render_type_ref(inner)))
            .unwrap_or_else(|| "!".to_string()),
        "LIST" => type_ref
            .of_type
            .as_deref()
            .map(|inner| format!("[{}]", render_type_ref(inner)))
            .unwrap_or_else(|| "[]".to_string()),
        _ => type_ref
            .name
            .clone()
            .unwrap_or_else(|| type_ref.kind.clone()),
    }
}

fn type_ref_from_introspection(type_ref: &IntrospectionTypeRef) -> Option<InputTypeRef> {
    let mut is_list = false;
    let mut current = type_ref;
    loop {
        match current.kind.as_str() {
            "LIST" => {
                is_list = true;
                current = current.of_type.as_deref()?;
            }
            "NON_NULL" => {
                current = current.of_type.as_deref()?;
            }
            _ => {
                let name = current.name.clone()?;
                return Some(InputTypeRef { name, is_list });
            }
        }
    }
}

fn render_introspection_definition(type_def: &IntrospectionTypeDef) -> (DefType, String) {
    let name = type_def.name.as_deref().unwrap_or("UnnamedType");
    match type_def.kind.as_str() {
        "OBJECT" => {
            let mut lines = vec![format!("type {name} {{")];
            for field in type_def.fields.as_deref().unwrap_or(&[]) {
                let args = if field.args.is_empty() {
                    String::new()
                } else {
                    let rendered = field
                        .args
                        .iter()
                        .map(|arg| format!("{}: {}", arg.name, render_type_ref(&arg.value_type)))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("({rendered})")
                };
                lines.push(format!(
                    "  {}{}: {}",
                    field.name,
                    args,
                    render_type_ref(&field.field_type)
                ));
            }
            lines.push("}".to_string());
            (DefType::Object, lines.join("\n"))
        }
        "INTERFACE" => {
            let mut lines = vec![format!("interface {name} {{")];
            for field in type_def.fields.as_deref().unwrap_or(&[]) {
                lines.push(format!(
                    "  {}: {}",
                    field.name,
                    render_type_ref(&field.field_type)
                ));
            }
            lines.push("}".to_string());
            (DefType::Interface, lines.join("\n"))
        }
        "INPUT_OBJECT" => {
            let mut lines = vec![format!("input {name} {{")];
            for field in type_def.input_fields.as_deref().unwrap_or(&[]) {
                lines.push(format!(
                    "  {}: {}",
                    field.name,
                    render_type_ref(&field.value_type)
                ));
            }
            lines.push("}".to_string());
            (DefType::Input, lines.join("\n"))
        }
        "ENUM" => {
            let mut lines = vec![format!("enum {name} {{")];
            for value in type_def.enum_values.as_deref().unwrap_or(&[]) {
                lines.push(format!("  {}", value.name));
            }
            lines.push("}".to_string());
            (DefType::Enum, lines.join("\n"))
        }
        "SCALAR" => (DefType::Other, format!("scalar {name}")),
        _ => (
            DefType::Other,
            format!("{} {}", type_def.kind.to_lowercase(), name),
        ),
    }
}

fn unwrap_named_type(t: &Type<String>) -> Option<String> {
    match t {
        Type::NamedType(n) => Some(n.clone()),
        Type::ListType(inner) => unwrap_named_type(inner),
        Type::NonNullType(inner) => unwrap_named_type(inner),
    }
}

fn type_ref_from_schema_type(t: &Type<String>) -> InputTypeRef {
    let mut is_list = false;
    let mut current = t;
    loop {
        match current {
            Type::NamedType(name) => {
                return InputTypeRef {
                    name: name.clone(),
                    is_list,
                };
            }
            Type::ListType(inner) => {
                is_list = true;
                current = inner;
            }
            Type::NonNullType(inner) => {
                current = inner;
            }
        }
    }
}

fn find_arg_type(field: &Field<String>, arg_name: &str) -> Option<String> {
    for arg in &field.arguments {
        if arg.name == arg_name {
            return unwrap_named_type(&arg.value_type);
        }
    }
    None
}

fn find_introspection_arg_type(field: &IntrospectionField, arg_name: &str) -> Option<String> {
    field
        .args
        .iter()
        .find(|arg| arg.name == arg_name)
        .and_then(|arg| named_type_name(&arg.value_type))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const TEST_SCHEMA: &str = include_str!("../schemas/consumer_schema.graphql");

    #[test]
    fn domain_config_is_derived_from_schema() {
        let registry = SchemaRegistry::new(TEST_SCHEMA);
        let cfg = registry.domain_config();

        assert!(
            !cfg.id_fields.is_empty(),
            "expected at least one id-like field derived from schema"
        );
        assert!(
            !cfg.numeric_fields.is_empty(),
            "expected at least one numeric field derived from schema"
        );
        assert!(
            !cfg.enum_values.is_empty(),
            "expected enum values derived from schema"
        );
    }

    #[test]
    fn root_filter_metadata_is_available() {
        let registry = SchemaRegistry::new(TEST_SCHEMA);
        let root = "queryVessel";
        let id_candidates = registry.root_identifier_filter_fields(root);
        let filter_object_fields = registry.root_filter_object_fields(root);

        assert!(
            !id_candidates.is_empty(),
            "expected identifier filter candidates for `{root}`"
        );
        assert!(
            !filter_object_fields.is_empty(),
            "expected filter-object fields for `{root}`"
        );
    }

    #[test]
    fn root_arg_metadata_is_available() {
        let registry = SchemaRegistry::new(TEST_SCHEMA);

        let get_args = registry.root_arg_names("getOffshoreWindFarm");
        assert!(get_args.contains(&"locationId".to_string()));
        assert!(get_args.contains(&"shortName".to_string()));

        let batch_arg = registry
            .root_arg_type_ref("batchGetOffshoreWindFarm", "shortNames")
            .expect("expected batch root arg type");
        assert_eq!(batch_arg.name, "String");
        assert!(batch_arg.is_list);
    }

    #[test]
    fn root_time_filter_metadata_is_available_for_temporal_series() {
        let registry = SchemaRegistry::new(TEST_SCHEMA);
        let root = "queryHistoricalScadaAgg10min";
        let time_fields = registry.root_time_filter_fields(root);
        assert!(
            !time_fields.is_empty(),
            "expected time-like filter fields for `{root}`"
        );
    }

    #[test]
    fn geo_object_roles_are_derived_from_coordinate_bearing_object_shapes() {
        let registry = SchemaRegistry::new(TEST_SCHEMA);
        let roles = registry.field_roles_for_root("queryOffshoreWindTurbine");

        assert!(
            roles
                .geo_object_fields
                .iter()
                .any(|field| field == "location"),
            "expected `location` to be recognized as a geo object field from schema shape, got {:?}",
            roles.geo_object_fields
        );
    }

    #[test]
    fn paired_scalar_coordinates_are_derived_from_schema_shape() {
        let registry = SchemaRegistry::new(TEST_SCHEMA);
        let roles = registry.field_roles_for_root("queryHistoricalAisVesselpos");

        assert!(
            roles.latitude_fields.iter().any(|field| field == "lat"),
            "expected paired scalar latitude field from schema shape, got {:?}",
            roles.latitude_fields
        );
        assert!(
            roles.longitude_fields.iter().any(|field| field == "lon"),
            "expected paired scalar longitude field from schema shape, got {:?}",
            roles.longitude_fields
        );
    }

    #[test]
    fn schema_search_uses_query_tokens_without_manual_keyword_expansion() {
        let registry = SchemaRegistry::new(TEST_SCHEMA);
        let result = registry.search("vessel position time");

        assert!(
            result.contains("type Vessel") || result.contains("queryVessel"),
            "expected schema search to find relevant definitions from query tokens, got {result}"
        );
    }

    #[test]
    fn best_matching_query_roots_prefers_weather_prediction_for_wind_trend_queries() {
        let registry = SchemaRegistry::new(TEST_SCHEMA);
        let roots = registry.best_matching_query_roots("Show wind speed trend over time", 3);

        assert_eq!(
            roots.first().map(String::as_str),
            Some("queryWeatherPrediction"),
            "expected weather prediction root to lead for wind-speed trend queries, got {roots:?}"
        );
    }

    #[test]
    fn planner_context_surfaces_relation_fields_for_parent_aggregate_queries() {
        let registry = SchemaRegistry::new(TEST_SCHEMA);
        let context = registry.planner_context("How many turbines does each wind farm have?", 4, 8);

        assert!(
            context.contains("queryOffshoreWindFarm"),
            "expected wind-farm root in planner context, got {context}"
        );
        assert!(
            context.contains("hasOffshoreWindTurbine.shortName")
                || context.contains("hasOffshoreWindTurbine.name"),
            "expected planner context to surface parent relation leaf paths, got {context}"
        );
        assert!(
            context.contains("intent_fields:"),
            "expected planner context to include intent-aware field preview, got {context}"
        );
    }

    #[test]
    fn planner_context_omits_relation_fields_for_plain_detail_lookups() {
        let registry = SchemaRegistry::new(TEST_SCHEMA);
        let context =
            registry.planner_context("Show details for wind farm shortName \"WF3\".", 4, 8);

        assert!(
            !context.contains("hasOffshoreWindTurbine.shortName")
                && !context.contains("hasOffshoreWindTurbine.name"),
            "did not expect child relation leaf paths in plain detail lookup context, got {context}"
        );
    }

    #[test]
    fn anchored_planner_context_surfaces_structural_relation_fields_without_scores() {
        let registry = SchemaRegistry::new(TEST_SCHEMA);
        let context = registry.anchored_planner_context(
            "List the first 10 turbines in wind farm \"Wind Farm 1\".",
            &[
                "queryOffshoreWindFarm".to_string(),
                "queryOffshoreWindTurbine".to_string(),
            ],
            8,
        );

        assert!(
            context.contains("Entity-anchored schema slice"),
            "expected anchored planner context header, got {context}"
        );
        assert!(
            context.contains("queryOffshoreWindFarm"),
            "expected anchored farm root in planner context, got {context}"
        );
        assert!(
            context.contains("hasOffshoreWindTurbine."),
            "expected anchored relation leaf path for farm membership, got {context}"
        );
        assert!(
            !context.contains("(score: "),
            "did not expect retrieval scores in anchored planner context, got {context}"
        );
    }

    #[test]
    fn anchored_planner_context_uses_collection_relations_for_structural_neighbors() {
        let registry = SchemaRegistry::new(TEST_SCHEMA);
        let turbine_location = registry
            .object_field_type_ref("OffshoreWindTurbine", "location")
            .expect("expected turbine location field metadata");
        let farm_turbines = registry
            .object_field_type_ref("OffshoreWindFarm", "hasOffshoreWindTurbine")
            .expect("expected farm turbine relation metadata");

        assert!(
            !turbine_location.is_list,
            "expected turbine location to be a singular relation"
        );
        assert!(
            farm_turbines.is_list,
            "expected farm turbines to be a collection relation"
        );

        let context = registry.anchored_planner_context(
            "Show details for turbine with shortName \"T3\".",
            &["queryOffshoreWindTurbine".to_string()],
            8,
        );

        assert!(
            !context.contains("location."),
            "did not expect singular location relation paths in anchored detail context, got {context}"
        );
    }

    #[test]
    fn planner_context_surfaces_time_and_numeric_capabilities_for_temporal_queries() {
        let registry = SchemaRegistry::new(TEST_SCHEMA);
        let context = registry.planner_context("Show wind speed trend over time", 3, 6);

        assert!(
            context.contains("queryWeatherPrediction"),
            "expected temporal weather root in planner context, got {context}"
        );
        assert!(
            context.contains("numeric_fields:"),
            "expected numeric field preview in planner context, got {context}"
        );
        assert!(
            context.contains("time_fields:"),
            "expected time field preview in planner context, got {context}"
        );
        assert!(
            context.contains("time_filter_fields:"),
            "expected time filter preview in planner context, got {context}"
        );
    }

    #[test]
    fn planner_context_prefers_relevant_filter_fields_over_full_filter_dump() {
        let registry = SchemaRegistry::new(TEST_SCHEMA);
        let context = registry.planner_context(
            "Compare average wind speed between Wind Farm 3 and Wind Farm 4 over time",
            4,
            8,
        );

        assert!(
            context.contains("identifier_filter_fields:") || context.contains("filter_fields:"),
            "expected filter preview in planner context, got {context}"
        );
        assert!(
            context.contains("timeFilter")
                || context.contains("timestamp")
                || context.contains("time"),
            "expected time-relevant filter hint in planner context, got {context}"
        );
    }

    #[test]
    fn planner_retrieval_budget_is_tighter_for_simple_lookup_queries() {
        let registry = SchemaRegistry::new(TEST_SCHEMA);
        let budget = registry.planner_retrieval_budget("List turbines");

        assert_eq!(
            budget,
            PlannerRetrievalBudget {
                root_limit: 3,
                field_limit: 6,
                entity_resolution_limit: 4,
            }
        );
    }

    #[test]
    fn planner_retrieval_budget_expands_for_analytical_temporal_queries() {
        let registry = SchemaRegistry::new(TEST_SCHEMA);
        let budget = registry.planner_retrieval_budget(
            "Compare average wind speed between Wind Farm 3 and Wind Farm 4 over time",
        );

        assert!(budget.root_limit >= 6, "unexpected root limit: {budget:?}");
        assert!(
            budget.field_limit >= 12,
            "unexpected field limit: {budget:?}"
        );
        assert!(
            budget.entity_resolution_limit >= 7,
            "unexpected entity resolution limit: {budget:?}"
        );
    }

    #[test]
    fn query_root_retrieval_profile_marks_specific_trend_query_as_high_confidence() {
        let registry = SchemaRegistry::new(TEST_SCHEMA);
        let profile = registry.query_root_retrieval_profile("Show wind speed trend over time", 4);

        assert_eq!(
            profile.confidence,
            RetrievalConfidence::High,
            "unexpected retrieval profile: {profile:?}"
        );
        assert_eq!(
            profile.matches.first().map(|entry| entry.root.as_str()),
            Some("queryWeatherPrediction")
        );
    }

    #[test]
    fn query_root_retrieval_profile_marks_generic_detail_query_as_low_confidence() {
        let registry = SchemaRegistry::new(TEST_SCHEMA);
        let profile = registry.query_root_retrieval_profile("Show details", 4);

        assert_eq!(profile.confidence, RetrievalConfidence::Low);
    }

    #[test]
    fn planner_context_surfaces_retrieval_confidence_and_root_scores() {
        let registry = SchemaRegistry::new(TEST_SCHEMA);
        let context = registry.planner_context("Show wind speed trend over time", 3, 6);

        assert!(
            context.contains("retrieval_confidence:"),
            "expected retrieval confidence in planner context, got {context}"
        );
        assert!(
            context.contains("(score: "),
            "expected root scores in planner context, got {context}"
        );
    }

    #[test]
    fn schema_retrieval_slice_exposes_compact_intent_specific_fields() {
        let registry = SchemaRegistry::new(TEST_SCHEMA);
        let slice = registry.schema_retrieval_slice("Show wind speed trend over time", 3, 4);

        assert_eq!(slice.profile.confidence, RetrievalConfidence::High);
        assert_eq!(slice.roots.len(), 3);
        let top = slice.roots.first().expect("expected top retrieval root");
        assert_eq!(top.root, "queryWeatherPrediction");
        assert!(top.time_fields.len() <= 4);
        assert!(top.numeric_fields.len() <= 4);
        assert!(
            top.time_fields.iter().any(|field| field == "time"),
            "expected trend slice to include time fields, got {top:?}"
        );
        assert!(
            !top.numeric_fields.is_empty(),
            "expected trend slice to include numeric fields, got {top:?}"
        );
    }

    #[test]
    fn sls_field_roles_are_merged_into_domain_config() {
        let bootstrap = SchemaRegistry::new(TEST_SCHEMA);
        let sls = crate::sls::load_sls_merged(&bootstrap, "sls.yaml").expect("load sls");
        let registry = SchemaRegistry::with_sls(TEST_SCHEMA, Some(&sls));
        let cfg = registry.domain_config();

        assert!(
            cfg.entity_key_fields.iter().any(|f| f == "mmsi"),
            "expected SLS entity_key_fields to be merged into domain config"
        );
        assert!(
            cfg.label_fields.iter().any(|f| f == "name"),
            "expected current SLS label_fields to be merged into domain config"
        );
    }

    #[test]
    fn sls_root_field_roles_override_defaults() {
        let bootstrap = SchemaRegistry::new(TEST_SCHEMA);
        let sls = crate::sls::load_sls_merged(&bootstrap, "sls.yaml").expect("load sls");
        let registry = SchemaRegistry::with_sls(TEST_SCHEMA, Some(&sls));
        let roles = registry.field_roles_for_root("queryWeatherPrediction");

        assert!(
            roles.label_fields.iter().any(|f| f == "location"),
            "expected SLS root override to include `location` label field"
        );
    }

    #[test]
    fn sls_substation_roles_prefer_id_for_compact_identifiers() {
        let bootstrap = SchemaRegistry::new(TEST_SCHEMA);
        let sls = crate::sls::load_sls_merged(&bootstrap, "sls.yaml").expect("load sls");
        let registry = SchemaRegistry::with_sls(TEST_SCHEMA, Some(&sls));

        for root in ["queryOffshoreSubstation", "queryOnshoreSubstation"] {
            let roles = registry.field_roles_for_root(root);
            assert_eq!(
                roles.id_fields.first().map(String::as_str),
                Some("id"),
                "expected {root} to prefer id as stable lookup field, got {:?}",
                roles.id_fields
            );
            assert!(
                roles
                    .entity_key_fields
                    .iter()
                    .any(|field| field == "shortName"),
                "expected {root} to keep shortName as a secondary entity key"
            );
        }
    }

    #[test]
    fn schema_scalar_types_enrich_weather_prediction_time_roles() {
        let bootstrap = SchemaRegistry::new(TEST_SCHEMA);
        let sls = crate::sls::load_sls_merged(&bootstrap, "sls.yaml").expect("load sls");
        let registry = SchemaRegistry::with_sls(TEST_SCHEMA, Some(&sls));
        let roles = registry.field_roles_for_root("queryWeatherPrediction");

        assert!(
            roles.time_fields.iter().any(|f| f == "time"),
            "expected schema scalar types to enrich weather prediction time roles, got {:?}",
            roles.time_fields
        );
    }

    #[test]
    fn schema_time_roles_expose_timestamp_filter_metadata() {
        let bootstrap = SchemaRegistry::new(TEST_SCHEMA);
        let sls = crate::sls::load_sls_merged(&bootstrap, "sls.yaml").expect("load sls");
        let registry = SchemaRegistry::with_sls(TEST_SCHEMA, Some(&sls));
        let time_fields = registry.root_time_filter_fields("queryHistoricalAisVesselpos");

        assert!(
            time_fields.iter().any(|field| field == "messageTimestamp"),
            "expected schema-derived timestamp filter metadata, got {:?}",
            time_fields
        );
    }

    #[test]
    fn validate_query_rejects_invalid_filter_operator() {
        let registry = SchemaRegistry::new(TEST_SCHEMA);
        let query = r#"query {
            queryVessel(filter: { name: { contains: "Alpha" } }) {
                name
            }
        }"#;
        let err = registry
            .validate_query(query)
            .expect_err("should reject invalid operator");
        assert!(err.to_string().contains("contains"));
        assert!(err.to_string().contains("StringHashFilter"));
    }

    #[test]
    fn validate_query_rejects_conflicting_order() {
        let registry = SchemaRegistry::new(TEST_SCHEMA);
        let query = r#"query {
            queryVessel(order: { asc: name, desc: mmsi }) {
                name
            }
        }"#;
        let err = registry
            .validate_query(query)
            .expect_err("should reject order conflict");
        assert!(err.to_string().contains("asc"));
        assert!(err.to_string().contains("desc"));
    }

    #[test]
    fn registry_builds_from_live_introspection_response() {
        let response = json!({
            "data": {
                "__schema": {
                    "queryType": { "name": "RootQuery" },
                    "types": [
                        {
                            "kind": "OBJECT",
                            "name": "RootQuery",
                            "fields": [
                                {
                                    "name": "queryOffshoreWindFarm",
                                    "args": [
                                        {
                                            "name": "filter",
                                            "type": { "kind": "INPUT_OBJECT", "name": "OffshoreWindFarmFilter", "ofType": null }
                                        },
                                        {
                                            "name": "order",
                                            "type": { "kind": "INPUT_OBJECT", "name": "OffshoreWindFarmOrder", "ofType": null }
                                        }
                                    ],
                                    "type": {
                                        "kind": "LIST",
                                        "name": null,
                                        "ofType": { "kind": "OBJECT", "name": "OffshoreWindFarm", "ofType": null }
                                    }
                                }
                            ]
                        },
                        {
                            "kind": "OBJECT",
                            "name": "OffshoreWindFarm",
                            "fields": [
                                {
                                    "name": "name",
                                    "args": [],
                                    "type": { "kind": "SCALAR", "name": "String", "ofType": null }
                                },
                                {
                                    "name": "shortName",
                                    "args": [],
                                    "type": { "kind": "SCALAR", "name": "String", "ofType": null }
                                }
                            ]
                        },
                        {
                            "kind": "INPUT_OBJECT",
                            "name": "OffshoreWindFarmFilter",
                            "inputFields": [
                                {
                                    "name": "shortName",
                                    "type": { "kind": "INPUT_OBJECT", "name": "StringHashFilter", "ofType": null }
                                }
                            ]
                        },
                        {
                            "kind": "INPUT_OBJECT",
                            "name": "StringHashFilter",
                            "inputFields": [
                                {
                                    "name": "contains",
                                    "type": { "kind": "SCALAR", "name": "String", "ofType": null }
                                }
                            ]
                        },
                        {
                            "kind": "INPUT_OBJECT",
                            "name": "OffshoreWindFarmOrder",
                            "inputFields": [
                                {
                                    "name": "asc",
                                    "type": { "kind": "ENUM", "name": "OffshoreWindFarmOrderable", "ofType": null }
                                }
                            ]
                        },
                        {
                            "kind": "ENUM",
                            "name": "OffshoreWindFarmOrderable",
                            "enumValues": [
                                { "name": "name" },
                                { "name": "shortName" }
                            ]
                        },
                        {
                            "kind": "SCALAR",
                            "name": "String"
                        }
                    ]
                }
            }
        });

        let registry =
            SchemaRegistry::from_introspection_response(&response, None).expect("introspection");

        assert_eq!(registry.schema_source(), SchemaSource::LiveIntrospection);
        assert!(
            registry
                .root_fields()
                .iter()
                .any(|root| root == "queryOffshoreWindFarm"),
            "expected live introspection to recover query roots"
        );
        assert_eq!(
            registry.query_filter_input("queryOffshoreWindFarm"),
            Some("OffshoreWindFarmFilter")
        );
        assert_eq!(
            registry.query_order_input("queryOffshoreWindFarm"),
            Some("OffshoreWindFarmOrder")
        );
        assert!(
            registry
                .object_field_names("OffshoreWindFarm")
                .expect("live object field names")
                .contains("shortName"),
            "expected live introspection to recover return type fields"
        );
    }

    #[test]
    fn wind_farm_default_fields_prefer_human_detail_fields_over_location_metadata() {
        let bootstrap = SchemaRegistry::new(include_str!("../schemas/consumer_schema.graphql"));
        let sls = crate::sls::load_sls_merged(&bootstrap, "sls.yaml").expect("load sls");
        let registry = SchemaRegistry::with_sls(
            include_str!("../schemas/consumer_schema.graphql"),
            Some(&sls),
        );

        let fields = registry.default_scalar_fields_for_root("queryOffshoreWindFarm", 6);

        assert!(
            fields.iter().any(|field| field == "name"),
            "expected name in preferred wind-farm fields: {fields:?}"
        );
        assert!(
            fields.iter().any(|field| field == "shortName"),
            "expected shortName in preferred wind-farm fields: {fields:?}"
        );
        assert!(
            fields.iter().any(|field| field == "plantId"),
            "expected plantId in preferred wind-farm fields: {fields:?}"
        );
        assert!(
            !fields.iter().any(|field| field == "locationId"),
            "did not expect locationId in top preferred wind-farm fields: {fields:?}"
        );
        assert!(
            !fields.iter().any(|field| field == "locationLabel"),
            "did not expect locationLabel in top preferred wind-farm fields: {fields:?}"
        );
    }
}
