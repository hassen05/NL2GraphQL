use crate::AppState;
use crate::agent::execute_graphql;
use crate::intermediate_representation::{IRQuery, graphql_value, ir_to_graphql};
use crate::planner_cache::{GroundingCacheKey, GroundingQuestionCacheKey};
use crate::schema_registry::{
    InputTypeRef, QueryRootRetrievalProfile, RetrievalConfidence, SchemaRegistry,
};
use crate::sls::IntentVocabulary;
use regex::Regex;
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};

const DEFAULT_GROUNDED_MATCHES_PER_MENTION: usize = 4;
const MIN_GROUNDED_MATCHES_PER_MENTION: usize = 2;
const MAX_ADAPTIVE_GROUNDED_MATCHES_PER_MENTION: usize = 6;
const DEFAULT_ROOT_GUIDED_LABEL_ROOTS: usize = 12;
const MIN_ROOT_GUIDED_LABEL_ROOTS: usize = 8;
const MAX_ADAPTIVE_ROOT_GUIDED_LABEL_ROOTS: usize = 16;
const SCHEMA_CANDIDATE_NOTE: &str = "Schema-derived candidate families are shown for this mention.";
const SCHEMA_ONLY_GROUNDING_NOTE: &str =
    "Backend grounding not attempted in schema-only resolution mode.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct GroundingBudget {
    schema_candidate_limit: usize,
    prioritized_root_limit: usize,
    root_guided_label_root_limit: usize,
    grounded_match_limit: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct EntityFamily {
    pub(crate) type_name: String,
    pub(crate) lookup_roots: Vec<String>,
    pub(crate) key_fields: Vec<String>,
    pub(crate) label_fields: Vec<String>,
    pub(crate) filter_fields: Vec<String>,
    pub(crate) display_fields: Vec<String>,
    pub(crate) relation_fields: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) enum ResolutionStatus {
    Grounded,
    SchemaCandidate,
    Ambiguous,
    Unresolved,
}

impl ResolutionStatus {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Grounded => "grounded",
            Self::SchemaCandidate => "schema_candidate",
            Self::Ambiguous => "ambiguous",
            Self::Unresolved => "unresolved",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct SchemaEntityCandidate {
    pub(crate) family_type: String,
    pub(crate) lookup_roots: Vec<String>,
    pub(crate) key_fields: Vec<String>,
    pub(crate) label_fields: Vec<String>,
    #[serde(skip_serializing)]
    pub(crate) filter_fields: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct GroundedEntityMatch {
    pub(crate) mention: String,
    pub(crate) family_type: String,
    pub(crate) root_field: String,
    pub(crate) matched_field: String,
    pub(crate) matched_value: String,
    pub(crate) stable_key_field: Option<String>,
    pub(crate) stable_key_value: Option<String>,
    pub(crate) canonical_value: String,
    pub(crate) display_label: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct EntityResolution {
    pub(crate) mention: String,
    pub(crate) status: ResolutionStatus,
    pub(crate) grounded_matches: Vec<GroundedEntityMatch>,
    pub(crate) schema_candidates: Vec<SchemaEntityCandidate>,
    pub(crate) notes: Vec<String>,
}

fn regex_alternation_pattern(values: &[String]) -> Option<String> {
    let mut ordered = values
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    ordered.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    ordered.dedup();
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

fn alias_looks_like_abbreviation(alias: &str) -> bool {
    let trimmed = alias.trim();
    trimmed.len() <= 4
        && trimmed
            .chars()
            .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit())
}

fn schema_identifier_mention_fields(catalog: &[EntityFamily]) -> Vec<String> {
    let mut fields = Vec::new();
    for family in catalog {
        for field in family.key_fields.iter().chain(family.label_fields.iter()) {
            if !family
                .filter_fields
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(field))
            {
                continue;
            }
            if fields.iter().any(|existing: &String| existing == field) {
                continue;
            }
            fields.push(field.clone());
        }
    }
    fields.sort();
    fields.dedup();
    fields
}

fn schema_entity_mention_phrases(
    schema_registry: &SchemaRegistry,
    catalog: &[EntityFamily],
) -> Vec<String> {
    let mut phrases = Vec::new();
    for family in catalog {
        for alias in schema_registry.concept_aliases_for_type(&family.type_name) {
            let trimmed = alias.trim();
            if trimmed.is_empty() {
                continue;
            }
            let has_alpha = trimmed.chars().any(|ch| ch.is_ascii_alphabetic());
            if !has_alpha || alias_looks_like_abbreviation(trimmed) {
                continue;
            }
            if phrases
                .iter()
                .any(|existing: &String| existing.eq_ignore_ascii_case(trimmed))
            {
                continue;
            }
            phrases.push(trimmed.to_string());
        }
    }
    phrases.sort();
    phrases.dedup();
    phrases
}

fn extract_generic_mentions_from_text(user_message: &str) -> BTreeSet<String> {
    let mut mentions = BTreeSet::new();

    let quoted = Regex::new(r#""([^"]+)""#).expect("quoted regex");
    for caps in quoted.captures_iter(user_message) {
        if let Some(m) = caps.get(1) {
            let value = m.as_str().trim();
            if !value.is_empty() {
                mentions.insert(value.to_string());
            }
        }
    }

    let code_like =
        Regex::new(r"\b[A-Z]{1,}[A-Z0-9]*(?:[-_][A-Z0-9]+)+\b|\b[A-Z]{2,}\d+\b|\b[A-Z]\d+\b")
            .expect("code-like regex");
    for m in code_like.find_iter(user_message) {
        mentions.insert(m.as_str().trim().to_string());
    }

    mentions
}

fn extract_schema_cued_label_mentions(
    entity_phrases: &[String],
    vocabulary: &IntentVocabulary,
    user_message: &str,
) -> BTreeSet<String> {
    let mut mentions = BTreeSet::new();
    let Some(entity_pattern) = regex_alternation_pattern(entity_phrases) else {
        return mentions;
    };
    let entity_cue =
        Regex::new(&format!(r#"(?i:\b(?:{entity_pattern})\b)"#)).expect("schema entity cue regex");
    for m in entity_cue.find_iter(user_message) {
        let tail = &user_message[m.end()..];
        if let Some(label) = schema_cued_label_from_tail(entity_phrases, vocabulary, tail) {
            mentions.insert(label);
        }
    }
    mentions
}

fn schema_cued_label_from_tail(
    entity_phrases: &[String],
    vocabulary: &IntentVocabulary,
    tail: &str,
) -> Option<String> {
    let rest = tail.trim_start();
    if rest.is_empty() {
        return None;
    }
    if let Some(after_cue) = strip_label_introducer(rest, &vocabulary.label_cues) {
        return label_from_explicit_cue_tail(entity_phrases, vocabulary, after_cue);
    }
    if let Some(identifier) = compact_identifier_from_tail(rest) {
        return Some(identifier);
    }
    let _ = entity_phrases;
    None
}

fn strip_label_introducer<'a>(text: &'a str, label_cues: &[String]) -> Option<&'a str> {
    label_cues.iter().find_map(|cue| strip_word(text, cue))
}

fn normalize_extracted_label(label: &str) -> Option<String> {
    let trimmed = label.trim();
    if trimmed.is_empty() || !trimmed.chars().any(|ch| ch.is_ascii_alphabetic()) {
        return None;
    }
    Some(trimmed.to_string())
}

fn label_from_explicit_cue_tail(
    entity_phrases: &[String],
    vocabulary: &IntentVocabulary,
    tail: &str,
) -> Option<String> {
    let rest = tail.trim_start();
    if let Some(quoted) = quoted_label_from_tail(rest) {
        return normalize_extracted_label(quoted);
    }
    label_token_prefixes(rest, 4)
        .into_iter()
        .find_map(|(candidate, remaining)| {
            is_structural_label_boundary(entity_phrases, &vocabulary.entity_connectors, remaining)
                .then(|| normalize_extracted_label(&candidate))
                .flatten()
        })
}

fn quoted_label_from_tail(tail: &str) -> Option<&str> {
    let rest = tail.trim_start();
    let after_open = rest.strip_prefix('"')?;
    let end = after_open.find('"')?;
    Some(after_open[..end].trim())
}

fn compact_identifier_from_tail(tail: &str) -> Option<String> {
    let identifier =
        Regex::new(r"^(?:[A-Z]{1,}[A-Z0-9]*(?:[-_][A-Z0-9]+)+|[A-Z]{2,}\d+|[A-Z]\d+)\b")
            .expect("compact schema-cued identifier regex");
    identifier
        .find(tail.trim_start())
        .map(|m| m.as_str().trim().to_string())
}

fn label_token_prefixes(tail: &str, max_tokens: usize) -> Vec<(String, &str)> {
    let rest = tail.trim_start();
    let mut prefixes = Vec::new();
    let mut end = 0usize;
    for (idx, token_match) in Regex::new(r"[A-Za-z0-9][A-Za-z0-9'_-]*")
        .expect("schema-cued label token regex")
        .find_iter(rest)
        .enumerate()
    {
        if idx >= max_tokens {
            break;
        }
        if token_match.start() != end
            && !rest[end..token_match.start()]
                .chars()
                .all(char::is_whitespace)
        {
            break;
        }
        end = token_match.end();
        prefixes.push((rest[..end].trim().to_string(), &rest[end..]));
    }
    prefixes
}

fn is_structural_label_boundary(
    entity_phrases: &[String],
    entity_connectors: &[String],
    tail: &str,
) -> bool {
    let rest = tail.trim_start();
    if rest.is_empty() {
        return true;
    }
    if rest
        .chars()
        .next()
        .is_some_and(|ch| matches!(ch, '.' | '?' | '!' | ',' | ';' | ':'))
    {
        return true;
    }
    entity_connectors
        .iter()
        .filter_map(|connector| strip_word(rest, connector))
        .any(|after_connector| starts_with_entity_phrase(entity_phrases, after_connector))
}

fn strip_word<'a>(text: &'a str, word: &str) -> Option<&'a str> {
    let word = word.trim();
    if word.is_empty() {
        return None;
    }
    let prefix = text.get(..word.len())?;
    if !prefix.eq_ignore_ascii_case(word) {
        return None;
    }
    let rest = text.get(word.len()..)?;
    if rest
        .chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_whitespace())
    {
        Some(rest.trim_start())
    } else {
        None
    }
}

fn starts_with_entity_phrase(entity_phrases: &[String], text: &str) -> bool {
    entity_phrases.iter().any(|phrase| {
        let phrase = phrase.trim();
        !phrase.is_empty()
            && text
                .get(..phrase.len())
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(phrase))
            && text
                .get(phrase.len()..)
                .and_then(|rest| rest.chars().next())
                .is_none_or(|ch| !ch.is_ascii_alphanumeric())
    })
}

fn extract_generic_mentions(
    schema_registry: &SchemaRegistry,
    catalog: &[EntityFamily],
    user_message: &str,
) -> Vec<String> {
    let mut mentions = extract_generic_mentions_from_text(user_message);
    let entity_phrases = schema_entity_mention_phrases(schema_registry, catalog);
    let vocabulary = schema_registry.intent_vocabulary();
    mentions.extend(extract_schema_cued_label_mentions(
        &entity_phrases,
        vocabulary,
        user_message,
    ));

    let identifier_fields = schema_identifier_mention_fields(catalog);
    if let Some(field_pattern) = regex_alternation_pattern(&identifier_fields) {
        let field_cued = Regex::new(&format!(
            r#"(?i:\b(?:{field_pattern})\b)\s+(?:"([^"]+)"|([A-Z][A-Z0-9:_-]*\d[A-Z0-9:_-]*))"#
        ))
        .expect("field-cued mention regex");
        for caps in field_cued.captures_iter(user_message) {
            if let Some(value) = caps
                .get(1)
                .or_else(|| caps.get(2))
                .map(|m| m.as_str().trim())
                && !value.is_empty()
            {
                mentions.insert(value.to_string());
            }
        }

        if let Some(entity_pattern) = regex_alternation_pattern(&entity_phrases) {
            let field_connector_prefix = regex_alternation_pattern(&vocabulary.field_connectors)
                .map(|connector_pattern| format!(r#"(?:(?:{connector_pattern})\s+)?"#))
                .unwrap_or_default();
            let entity_cued = Regex::new(&format!(
                r#"(?i:\b(?:{entity_pattern})\b(?:\s+{field_connector_prefix}(?:{field_pattern}))?)\s+(?:"([^"]+)"|([A-Z]{{2,}}\d+|[A-Z][A-Za-z0-9_-]*(?:\s+[A-Z][A-Za-z0-9_-]*){{0,3}}(?:\s+\d+[A-Za-z0-9_-]*)?))"#
            ))
            .expect("entity-cued mention regex");
            for caps in entity_cued.captures_iter(user_message) {
                if let Some(value) = caps
                    .get(1)
                    .or_else(|| caps.get(2))
                    .map(|m| m.as_str().trim())
                    && !value.is_empty()
                {
                    mentions.insert(value.to_string());
                }
            }
        }
    }

    let mut ordered = mentions.into_iter().collect::<Vec<_>>();
    ordered.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    let mut filtered = Vec::new();
    for mention in ordered {
        if looks_like_non_entity_phrase(&mention, vocabulary) {
            continue;
        }
        if filtered.iter().any(|existing: &String| {
            existing.len() > mention.len()
                && existing
                    .to_ascii_lowercase()
                    .contains(&mention.to_ascii_lowercase())
        }) {
            continue;
        }
        filtered.push(mention);
    }
    filtered
}

fn looks_like_non_entity_phrase(mention: &str, vocabulary: &IntentVocabulary) -> bool {
    let tokens = normalized_text_tokens(mention);
    if tokens.is_empty() {
        return true;
    }
    if tokens.len() < 2 {
        return false;
    }
    vocabulary
        .rank_desc
        .iter()
        .chain(vocabulary.rank_asc.iter())
        .filter_map(|term| {
            let term_tokens = normalized_text_tokens(term);
            (!term_tokens.is_empty()).then_some(term_tokens)
        })
        .any(|term_tokens| {
            tokens.len() > term_tokens.len()
                && tokens[..term_tokens.len()] == term_tokens
                && tokens[term_tokens.len()]
                    .chars()
                    .all(|ch| ch.is_ascii_digit())
        })
}

pub(crate) fn extracted_entity_mentions(
    schema_registry: &SchemaRegistry,
    user_message: &str,
) -> Vec<String> {
    let catalog = build_entity_catalog(schema_registry);
    extract_generic_mentions(schema_registry, &catalog, user_message)
}

pub(crate) fn render_entity_mention_hints_block(
    schema_registry: &SchemaRegistry,
    user_message: &str,
) -> String {
    let catalog = build_entity_catalog(schema_registry);
    let mentions = extract_generic_mentions(schema_registry, &catalog, user_message);
    if mentions.is_empty() {
        return "Potential entity mentions from the user request: none".to_string();
    }

    let mut lines = vec![
        "Potential entity mentions from the user request (not resolved):".to_string(),
        "Use these only as raw text spans from the request. Choose the entity type, root, and filter field from the schema context; do not assume these mentions are already grounded."
            .to_string(),
    ];
    for mention in mentions {
        lines.push(format!("- mention=`{mention}`"));
    }
    lines.join("\n")
}

fn looks_like_compact_identifier(mention: &str) -> bool {
    let trimmed = mention.trim();
    if trimmed.is_empty() {
        return false;
    }
    let compacted: String = trimmed
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .collect();
    if compacted.is_empty() {
        return false;
    }
    let has_digit = compacted.chars().any(|c| c.is_ascii_digit());
    let has_sep = compacted.contains('-') || compacted.contains('_') || compacted.contains(':');
    let all_compact = compacted.chars().all(|c| {
        c.is_ascii_uppercase()
            || c.is_ascii_lowercase()
            || c.is_ascii_digit()
            || matches!(c, '-' | '_' | ':')
    });
    let has_alpha = compacted.chars().any(|c| c.is_ascii_alphabetic());
    if trimmed.contains(char::is_whitespace) {
        let token_count = trimmed
            .split_whitespace()
            .filter(|token| !token.trim().is_empty())
            .count();
        let uppercase_alpha = compacted
            .chars()
            .filter(|c| c.is_ascii_alphabetic())
            .all(|c| c.is_ascii_uppercase());
        return token_count <= 3
            && all_compact
            && has_alpha
            && has_digit
            && (has_sep || uppercase_alpha);
    }
    all_compact && has_alpha && (has_digit || has_sep)
}

fn grounding_budget_for_query_shape(
    user_message: &str,
    mention: Option<&str>,
    base_schema_candidate_limit: usize,
) -> GroundingBudget {
    let token_count = user_message
        .split_whitespace()
        .filter(|token| !token.trim().is_empty())
        .count();
    let compact_identifier = mention.is_some_and(looks_like_compact_identifier);
    let has_quoted_span = user_message.matches('"').count() >= 2;
    let mention_count = extract_generic_mentions_from_text(user_message).len();
    let identifier_like_mentions = extract_generic_mentions_from_text(user_message)
        .into_iter()
        .filter(|candidate| looks_like_compact_identifier(candidate))
        .count();

    let mut schema_candidate_limit = base_schema_candidate_limit.max(1);
    let mut prioritized_root_limit = 8usize;
    let mut root_guided_label_root_limit = DEFAULT_ROOT_GUIDED_LABEL_ROOTS;
    let mut grounded_match_limit = DEFAULT_GROUNDED_MATCHES_PER_MENTION;

    if token_count >= 10 {
        schema_candidate_limit += 1;
        prioritized_root_limit += 1;
        root_guided_label_root_limit += 1;
    }
    if mention_count >= 2 {
        schema_candidate_limit += 1;
        prioritized_root_limit += 1;
        root_guided_label_root_limit += 1;
    }
    if has_quoted_span {
        schema_candidate_limit += 1;
        grounded_match_limit += 1;
    }
    if identifier_like_mentions >= 2 {
        grounded_match_limit += 1;
    }
    if compact_identifier {
        schema_candidate_limit = schema_candidate_limit.min(base_schema_candidate_limit.max(1));
        prioritized_root_limit = prioritized_root_limit.min(6);
        root_guided_label_root_limit = root_guided_label_root_limit.min(10);
        grounded_match_limit = grounded_match_limit.min(3);
    }

    GroundingBudget {
        schema_candidate_limit: schema_candidate_limit.clamp(3, 10),
        prioritized_root_limit: prioritized_root_limit.clamp(6, 12),
        root_guided_label_root_limit: root_guided_label_root_limit.clamp(
            MIN_ROOT_GUIDED_LABEL_ROOTS,
            MAX_ADAPTIVE_ROOT_GUIDED_LABEL_ROOTS,
        ),
        grounded_match_limit: grounded_match_limit.clamp(
            MIN_GROUNDED_MATCHES_PER_MENTION,
            MAX_ADAPTIVE_GROUNDED_MATCHES_PER_MENTION,
        ),
    }
}

fn grounding_budget_for_request(
    schema_registry: &SchemaRegistry,
    user_message: &str,
    mention: Option<&str>,
    base_schema_candidate_limit: usize,
) -> GroundingBudget {
    let compact_identifier = mention.is_some_and(looks_like_compact_identifier);
    let mention_count = extract_generic_mentions_from_text(user_message).len();
    let mut budget =
        grounding_budget_for_query_shape(user_message, mention, base_schema_candidate_limit);
    let retrieval_profile = schema_registry.query_root_retrieval_profile(user_message, 6);
    apply_retrieval_signal_to_grounding_budget(&mut budget, &retrieval_profile);
    if mention_count >= 2 && !compact_identifier {
        budget.root_guided_label_root_limit = budget
            .root_guided_label_root_limit
            .max(DEFAULT_ROOT_GUIDED_LABEL_ROOTS + 1);
        budget.grounded_match_limit = budget
            .grounded_match_limit
            .max(DEFAULT_GROUNDED_MATCHES_PER_MENTION + 1);
    }
    if compact_identifier {
        budget.prioritized_root_limit = budget.prioritized_root_limit.min(6);
        budget.root_guided_label_root_limit = budget.root_guided_label_root_limit.min(10);
        budget.grounded_match_limit = budget.grounded_match_limit.min(3);
    }
    budget
}

fn mention_matches_prefixed_digits(mention: &str, prefix: &str) -> bool {
    let lower = mention.trim().to_ascii_lowercase();
    lower.starts_with(prefix)
        && lower.len() > prefix.len()
        && lower[prefix.len()..].chars().all(|ch| ch.is_ascii_digit())
}

fn alias_identifier_prefixes(aliases: &[String]) -> Vec<String> {
    let mut prefixes = Vec::new();
    for alias in aliases {
        let trimmed = alias.trim();
        if trimmed.is_empty() {
            continue;
        }

        let normalized = normalized_field_name(trimmed);
        if (2..=5).contains(&normalized.len())
            && trimmed.chars().any(|ch| ch.is_ascii_uppercase())
            && trimmed.chars().all(|ch| ch.is_ascii_alphanumeric())
        {
            prefixes.push(normalized.clone());
        }

        let tokens = trimmed
            .split(|ch: char| !ch.is_ascii_alphanumeric())
            .filter(|token| !token.is_empty())
            .collect::<Vec<_>>();
        let initials = tokens
            .iter()
            .filter_map(|token| token.chars().next())
            .collect::<String>()
            .to_ascii_lowercase();
        if initials.len() >= 2 {
            prefixes.push(initials);
        }
    }

    prefixes.retain(|prefix| !prefix.is_empty());
    prefixes.sort();
    prefixes.dedup();
    prefixes
}

fn normalized_field_name(field: &str) -> String {
    field
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .map(|ch| ch.to_ascii_lowercase())
        .collect()
}

fn candidate_concept_aliases(
    schema_registry: &SchemaRegistry,
    candidate: &SchemaEntityCandidate,
) -> Vec<String> {
    schema_registry.concept_aliases_for_type(&candidate.family_type)
}

fn normalized_text_tokens(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|token| {
            token
                .trim_matches(|ch: char| !ch.is_ascii_alphanumeric())
                .to_ascii_lowercase()
        })
        .filter(|token| !token.is_empty())
        .collect()
}

fn token_sequence_contains_term(tokens: &[String], term_tokens: &[String]) -> bool {
    if term_tokens.is_empty() || term_tokens.len() > tokens.len() {
        return false;
    }
    tokens
        .windows(term_tokens.len())
        .any(|window| window == term_tokens)
}

fn message_mentions_vocabulary_terms(message: &str, terms: &[String]) -> bool {
    let message_tokens = normalized_text_tokens(message);
    if message_tokens.is_empty() {
        return false;
    }
    terms
        .iter()
        .map(|term| normalized_text_tokens(term))
        .any(|term_tokens| token_sequence_contains_term(&message_tokens, &term_tokens))
}

fn alias_directly_cues_mention(
    user_message: &str,
    mention: &str,
    alias: &str,
    label_cues: &[String],
) -> bool {
    let alias_tokens = normalized_text_tokens(alias);
    let mention_tokens = normalized_text_tokens(mention);
    if alias_tokens.is_empty() || mention_tokens.is_empty() {
        return false;
    }

    let message_tokens = normalized_text_tokens(user_message);
    let alias_len = alias_tokens.len();
    let mention_len = mention_tokens.len();
    if alias_len + mention_len > message_tokens.len() {
        return false;
    }

    for idx in 0..=message_tokens.len().saturating_sub(alias_len) {
        if message_tokens[idx..idx + alias_len] != alias_tokens {
            continue;
        }
        let after_alias = idx + alias_len;
        if after_alias + mention_len <= message_tokens.len()
            && message_tokens[after_alias..after_alias + mention_len] == mention_tokens
        {
            return true;
        }
        for cue_tokens in label_cues
            .iter()
            .map(|cue| normalized_text_tokens(cue))
            .filter(|tokens| !tokens.is_empty())
        {
            let cue_len = cue_tokens.len();
            if after_alias + cue_len + mention_len <= message_tokens.len()
                && message_tokens[after_alias..after_alias + cue_len] == cue_tokens
                && message_tokens[after_alias + cue_len..after_alias + cue_len + mention_len]
                    == mention_tokens
            {
                return true;
            }
        }
    }

    false
}

fn schema_field_cue_count<'a>(
    fields: impl IntoIterator<Item = &'a String>,
    user_message: &str,
) -> usize {
    let normalized_message = normalized_field_name(user_message);
    let message_tokens = normalized_text_tokens(user_message);
    let mut matched_fields = BTreeSet::new();

    for field in fields {
        let normalized_field = normalized_field_name(field);
        if normalized_field.len() < 4 {
            continue;
        }
        if normalized_message.contains(&normalized_field)
            || message_tokens.iter().any(|token| {
                token.len() >= 6
                    && (normalized_field.contains(token) || token.contains(&normalized_field))
            })
        {
            matched_fields.insert(normalized_field);
        }
    }

    matched_fields.len()
}

fn schema_candidate_score_for_mention(
    schema_registry: &SchemaRegistry,
    candidate: &SchemaEntityCandidate,
    user_message: &str,
    mention: &str,
) -> i32 {
    let lower_mention = mention.trim().to_ascii_lowercase();
    let aliases = candidate_concept_aliases(schema_registry, candidate);
    let compact_identifier = looks_like_compact_identifier(mention);
    let mut score = 0i32;

    for alias in &aliases {
        let phrase = alias.trim().to_ascii_lowercase();
        if phrase.is_empty() {
            continue;
        }
        if !alias_looks_like_abbreviation(alias)
            && alias_directly_cues_mention(
                user_message,
                mention,
                alias,
                &schema_registry.intent_vocabulary().label_cues,
            )
        {
            score += 8;
        }
        if !alias_looks_like_abbreviation(alias) && lower_mention.contains(&phrase) {
            score += 10;
        }
    }

    if compact_identifier {
        for prefix in alias_identifier_prefixes(&aliases) {
            if mention_matches_prefixed_digits(mention, &prefix) {
                score += 8;
            }
        }
    }

    let field_cue_count = schema_field_cue_count(
        candidate
            .key_fields
            .iter()
            .chain(candidate.label_fields.iter())
            .chain(candidate.filter_fields.iter()),
        user_message,
    );
    score += (field_cue_count as i32) * 3;

    let lookup_root_match = candidate.lookup_roots.iter().any(|root| {
        let normalized_root = normalized_field_name(root);
        aliases.iter().any(|alias| {
            let normalized_alias = normalized_field_name(alias);
            !normalized_alias.is_empty() && normalized_root.contains(&normalized_alias)
        })
    });
    if lookup_root_match {
        score += 1;
    }

    score
}

fn narrow_schema_candidates_for_mention(
    schema_registry: &SchemaRegistry,
    schema_candidates: Vec<SchemaEntityCandidate>,
    user_message: &str,
    mention: &str,
) -> (Vec<SchemaEntityCandidate>, Option<String>) {
    let best_score = schema_candidates
        .iter()
        .map(|candidate| {
            schema_candidate_score_for_mention(schema_registry, candidate, user_message, mention)
        })
        .max()
        .unwrap_or(0);
    if best_score <= 0 {
        return (schema_candidates, None);
    }

    let narrowed = schema_candidates
        .iter()
        .filter(|candidate| {
            schema_candidate_score_for_mention(schema_registry, candidate, user_message, mention)
                == best_score
        })
        .cloned()
        .collect::<Vec<_>>();
    if narrowed.is_empty() || narrowed.len() == schema_candidates.len() {
        return (schema_candidates, None);
    }

    let mut preferred_list = narrowed
        .iter()
        .map(|candidate| candidate.family_type.clone())
        .collect::<Vec<_>>();
    preferred_list.sort();
    preferred_list.dedup();

    (
        narrowed,
        Some(format!(
            "Context/type hints narrowed schema candidates to: {}.",
            preferred_list.join(", ")
        )),
    )
}

fn grounding_budget_for_resolution(
    schema_registry: &SchemaRegistry,
    user_message: &str,
    resolution: &EntityResolution,
    base_schema_candidate_limit: usize,
) -> GroundingBudget {
    let mut budget = grounding_budget_for_request(
        schema_registry,
        user_message,
        Some(&resolution.mention),
        base_schema_candidate_limit,
    );
    let candidate_count = resolution.schema_candidates.len();

    if resolution.status == ResolutionStatus::Ambiguous {
        budget.prioritized_root_limit += 1;
        budget.root_guided_label_root_limit += 2;
        budget.grounded_match_limit += 1;
    }
    if candidate_count >= 3 {
        budget.prioritized_root_limit += 1;
        budget.root_guided_label_root_limit += 1;
    }
    if candidate_count == 1 && resolution.status == ResolutionStatus::SchemaCandidate {
        budget.grounded_match_limit = budget
            .grounded_match_limit
            .min(DEFAULT_GROUNDED_MATCHES_PER_MENTION);
    }

    GroundingBudget {
        schema_candidate_limit: budget.schema_candidate_limit.clamp(3, 10),
        prioritized_root_limit: budget.prioritized_root_limit.clamp(6, 12),
        root_guided_label_root_limit: budget.root_guided_label_root_limit.clamp(
            MIN_ROOT_GUIDED_LABEL_ROOTS,
            MAX_ADAPTIVE_ROOT_GUIDED_LABEL_ROOTS,
        ),
        grounded_match_limit: budget.grounded_match_limit.clamp(
            MIN_GROUNDED_MATCHES_PER_MENTION,
            MAX_ADAPTIVE_GROUNDED_MATCHES_PER_MENTION,
        ),
    }
}

fn apply_retrieval_signal_to_grounding_budget(
    budget: &mut GroundingBudget,
    profile: &QueryRootRetrievalProfile,
) {
    match profile.confidence {
        RetrievalConfidence::High => {
            budget.prioritized_root_limit = budget.prioritized_root_limit.saturating_sub(1).max(4);
            budget.root_guided_label_root_limit = budget
                .root_guided_label_root_limit
                .saturating_sub(2)
                .max(MIN_ROOT_GUIDED_LABEL_ROOTS);
            budget.schema_candidate_limit = budget.schema_candidate_limit.saturating_sub(1).max(3);
        }
        RetrievalConfidence::Medium => {}
        RetrievalConfidence::Low => {
            budget.prioritized_root_limit += 1;
            budget.root_guided_label_root_limit += 2;
            budget.schema_candidate_limit += 1;
            if profile.competitive_root_count >= 4 {
                budget.prioritized_root_limit += 1;
                budget.grounded_match_limit += 1;
            }
        }
    }

    if profile.runner_up_score > 0 && profile.top_score - profile.runner_up_score <= 25 {
        budget.prioritized_root_limit += 1;
        budget.root_guided_label_root_limit += 1;
    }
    if profile.competitive_root_count >= 3 {
        budget.root_guided_label_root_limit += 1;
        budget.grounded_match_limit += 1;
    }

    budget.prioritized_root_limit = budget.prioritized_root_limit.clamp(4, 12);
    budget.root_guided_label_root_limit = budget.root_guided_label_root_limit.clamp(
        MIN_ROOT_GUIDED_LABEL_ROOTS,
        MAX_ADAPTIVE_ROOT_GUIDED_LABEL_ROOTS,
    );
    budget.schema_candidate_limit = budget.schema_candidate_limit.clamp(3, 10);
    budget.grounded_match_limit = budget.grounded_match_limit.clamp(
        MIN_GROUNDED_MATCHES_PER_MENTION,
        MAX_ADAPTIVE_GROUNDED_MATCHES_PER_MENTION,
    );
}

#[allow(clippy::collapsible_if)]
fn typed_lookup_json_value(type_name: &str, raw: &str) -> Value {
    let lower = type_name.to_ascii_lowercase();
    if (lower == "int" || lower.ends_with("int"))
        && let Ok(v) = raw.parse::<i64>()
    {
        return serde_json::json!(v);
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
    if (lower == "boolean" || lower == "bool")
        && let Ok(v) = raw.parse::<bool>()
    {
        return serde_json::json!(v);
    }
    serde_json::json!(raw)
}

fn build_exact_filter(
    schema_registry: &SchemaRegistry,
    root_field: &str,
    field: &str,
    raw_value: &str,
) -> Option<Value> {
    let type_ref = schema_registry.filter_field_type_ref(root_field, field)?;
    if let Some(op_fields) = schema_registry.input_field_names(&type_ref.name)
        && op_fields.iter().any(|op| op.eq_ignore_ascii_case("eq"))
    {
        let op_type = schema_registry.input_field_type_ref(&type_ref.name, "eq")?;
        let typed = if op_type.is_list {
            Value::Array(vec![typed_lookup_json_value(&op_type.name, raw_value)])
        } else {
            typed_lookup_json_value(&op_type.name, raw_value)
        };
        return Some(serde_json::json!({
            field: { "eq": typed }
        }));
    }
    let typed = if type_ref.is_list {
        Value::Array(vec![typed_lookup_json_value(&type_ref.name, raw_value)])
    } else {
        typed_lookup_json_value(&type_ref.name, raw_value)
    };
    Some(serde_json::json!({ field: typed }))
}

fn title_case_variant(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let titled = trimmed
        .split_whitespace()
        .map(|token| {
            let mut saw_alpha = false;
            token
                .chars()
                .map(|ch| {
                    if ch.is_ascii_alphabetic() {
                        if !saw_alpha {
                            saw_alpha = true;
                            ch.to_ascii_uppercase()
                        } else {
                            ch.to_ascii_lowercase()
                        }
                    } else {
                        ch
                    }
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join(" ");
    (!titled.eq(trimmed)).then_some(titled)
}

fn exact_label_lookup_variants(mention: &str) -> Vec<String> {
    let mut out = Vec::new();
    let trimmed = mention.trim();
    if trimmed.is_empty() {
        return out;
    }
    out.push(trimmed.to_string());
    if let Some(titled) = title_case_variant(trimmed)
        && !out.iter().any(|existing| existing == &titled)
    {
        out.push(titled);
    }
    out
}

fn exact_identifier_lookup_variants(mention: &str) -> Vec<String> {
    fn push_unique(out: &mut Vec<String>, value: String) {
        if !out.iter().any(|existing| existing == &value) {
            out.push(value);
        }
    }

    let trimmed = mention.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let mut out = vec![trimmed.to_string()];
    let Ok(re) = Regex::new(r"^(?P<prefix>.*?)(?P<num>\d+)$") else {
        return out;
    };
    if let Some(caps) = re.captures(trimmed) {
        let raw_prefix = caps.name("prefix").map(|m| m.as_str()).unwrap_or("");
        let prefix = raw_prefix.trim_end();
        let num = caps.name("num").map(|m| m.as_str()).unwrap_or("");
        if !prefix.is_empty() && !num.is_empty() {
            let normalized_num = num
                .parse::<u64>()
                .ok()
                .map(|n| n.to_string())
                .unwrap_or_else(|| num.to_string());
            let mut numeric_forms = vec![num.to_string()];
            push_unique(&mut numeric_forms, normalized_num);

            for numeric in numeric_forms {
                push_unique(&mut out, format!("{prefix}{numeric}"));
                let compact_prefix = prefix.trim_end_matches(['-', '_', ' ']);
                if compact_prefix != prefix && !compact_prefix.is_empty() {
                    push_unique(&mut out, format!("{compact_prefix}{numeric}"));
                }
                if numeric.len() < 3 {
                    for width in (numeric.len() + 1)..=3 {
                        push_unique(&mut out, format!("{prefix}{numeric:>width$}"));
                    }
                    push_unique(&mut out, format!("{prefix}{numeric:0>3}"));
                }
                if prefix.ends_with('-') && numeric.len() < 3 {
                    for width in 1..=3 {
                        push_unique(&mut out, format!("{prefix}{numeric:>width$}"));
                    }
                }
            }
        }
    }
    out
}

fn lookup_selection_fields(
    schema_registry: &SchemaRegistry,
    family: &EntityFamily,
    _root_field: &str,
    matched_field: &str,
) -> Vec<String> {
    let mut fields = Vec::new();
    let mut push_field = |field: &str| {
        let field = field.trim();
        if field.is_empty()
            || fields
                .iter()
                .any(|existing: &String| existing.eq_ignore_ascii_case(field))
        {
            return;
        }
        fields.push(field.to_string());
    };

    push_field(matched_field);
    let _ = schema_registry;
    let _ = family;
    fields
}

fn string_field_value(row: &Value, field: &str) -> Option<String> {
    row.get(field).and_then(|value| match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    })
}

fn field_is_role_backed_key(fields: &[String], field: &str) -> bool {
    fields
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(field))
}

fn stable_key_for_row(
    schema_registry: &SchemaRegistry,
    row: &Value,
    root_field: &str,
    matched_field: &str,
) -> (Option<String>, Option<String>) {
    let roles = schema_registry.field_roles_for_root(root_field);
    let mut candidates = Vec::new();
    let mut push_candidate = |field: &str| {
        if candidates
            .iter()
            .any(|existing: &String| existing.eq_ignore_ascii_case(field))
        {
            return;
        }
        candidates.push(field.to_string());
    };

    if field_is_role_backed_key(&roles.id_fields, matched_field) {
        push_candidate(matched_field);
    }
    for field in &roles.id_fields {
        push_candidate(field);
    }
    for field in &roles.entity_key_fields {
        push_candidate(field);
    }
    if field_is_role_backed_key(&roles.entity_key_fields, matched_field) {
        push_candidate(matched_field);
    }
    for field in schema_registry.root_identifier_filter_fields(root_field) {
        if field_is_role_backed_key(&roles.id_fields, &field)
            || field_is_role_backed_key(&roles.entity_key_fields, &field)
        {
            push_candidate(&field);
        }
    }

    for field in candidates {
        if let Some(value) = string_field_value(row, &field) {
            return (Some(field), Some(value));
        }
    }
    (None, None)
}

fn display_label_for_row(row: &Value, family: &EntityFamily) -> Option<String> {
    family
        .label_fields
        .iter()
        .find_map(|field| string_field_value(row, field))
}

fn grounded_match_from_row(
    schema_registry: &SchemaRegistry,
    row: &Value,
    family: &EntityFamily,
    root_field: &str,
    matched_field: &str,
    mention: &str,
) -> Option<GroundedEntityMatch> {
    let matched_value = string_field_value(row, matched_field)?;
    let (stable_key_field, stable_key_value) =
        stable_key_for_row(schema_registry, row, root_field, matched_field);
    Some(GroundedEntityMatch {
        mention: mention.to_string(),
        family_type: family.type_name.clone(),
        root_field: root_field.to_string(),
        matched_field: matched_field.to_string(),
        matched_value: matched_value.clone(),
        stable_key_field,
        stable_key_value,
        canonical_value: matched_value,
        display_label: display_label_for_row(row, family),
    })
}

fn descriptive_grounding_requires_confirmation(resolution: &mut EntityResolution, note: &str) {
    resolution.status = ResolutionStatus::SchemaCandidate;
    resolution.notes.push(note.to_string());
}

fn grounded_matches_same_entity(
    existing: &GroundedEntityMatch,
    candidate: &GroundedEntityMatch,
) -> bool {
    if existing.family_type != candidate.family_type {
        return false;
    }

    if let (
        Some(existing_field),
        Some(existing_value),
        Some(candidate_field),
        Some(candidate_value),
    ) = (
        existing.stable_key_field.as_ref(),
        existing.stable_key_value.as_ref(),
        candidate.stable_key_field.as_ref(),
        candidate.stable_key_value.as_ref(),
    ) && existing_field.eq_ignore_ascii_case(candidate_field)
        && existing_value.eq_ignore_ascii_case(candidate_value)
    {
        return true;
    }

    if let (Some(existing_label), Some(candidate_label)) = (
        existing.display_label.as_ref(),
        candidate.display_label.as_ref(),
    ) && existing_label.eq_ignore_ascii_case(candidate_label)
        && existing
            .matched_value
            .eq_ignore_ascii_case(&candidate.matched_value)
    {
        return true;
    }

    existing
        .matched_field
        .eq_ignore_ascii_case(&candidate.matched_field)
        && existing
            .matched_value
            .eq_ignore_ascii_case(&candidate.matched_value)
}

fn insert_grounded_match(
    grounded_matches: &mut Vec<GroundedEntityMatch>,
    grounded: GroundedEntityMatch,
) {
    if let Some(existing) = grounded_matches
        .iter_mut()
        .find(|existing| grounded_matches_same_entity(existing, &grounded))
    {
        if existing.stable_key_field.is_none() && grounded.stable_key_field.is_some() {
            existing.stable_key_field = grounded.stable_key_field.clone();
            existing.stable_key_value = grounded.stable_key_value.clone();
        }
        if existing.display_label.is_none() && grounded.display_label.is_some() {
            existing.display_label = grounded.display_label.clone();
        }
        if existing.root_field.starts_with("batchGet") && grounded.root_field.starts_with("get") {
            existing.root_field = grounded.root_field.clone();
        }
        return;
    }
    grounded_matches.push(grounded);
}

fn clear_schema_only_grounding_notes(notes: &mut Vec<String>) {
    notes.retain(|note| !note.contains("Backend grounding not attempted"));
}

fn rows_from_lookup_response(body: &Value, root_field: &str) -> Vec<Value> {
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

fn auth_parts(state: &AppState) -> (Option<&str>, Option<&str>, Option<&str>) {
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
    (bearer_token, api_key_header, api_key)
}

pub(crate) fn build_entity_catalog(schema_registry: &SchemaRegistry) -> Vec<EntityFamily> {
    let mut by_type: HashMap<String, EntityFamily> = HashMap::new();

    for root in schema_registry.root_fields() {
        if !root.starts_with("query") && !root.starts_with("get") && !root.starts_with("batchGet") {
            continue;
        }
        let Some(type_name) = schema_registry.query_return_type(&root) else {
            continue;
        };
        let roles = schema_registry.field_roles_for_root(&root);
        let entry = by_type
            .entry(type_name.to_string())
            .or_insert_with(|| EntityFamily {
                type_name: type_name.to_string(),
                lookup_roots: Vec::new(),
                key_fields: Vec::new(),
                label_fields: Vec::new(),
                filter_fields: Vec::new(),
                display_fields: Vec::new(),
                relation_fields: Vec::new(),
            });

        if !entry.lookup_roots.iter().any(|existing| existing == &root) {
            entry.lookup_roots.push(root.clone());
        }
        for field in roles.id_fields.iter().chain(roles.entity_key_fields.iter()) {
            if !entry.key_fields.iter().any(|existing| existing == field) {
                entry.key_fields.push(field.clone());
            }
        }
        for field in &roles.label_fields {
            if !entry.label_fields.iter().any(|existing| existing == field) {
                entry.label_fields.push(field.clone());
            }
        }
        for field in schema_registry.root_filter_fields(&root) {
            if !entry
                .filter_fields
                .iter()
                .any(|existing| existing == &field)
            {
                entry.filter_fields.push(field);
            }
        }
        for field in schema_registry.default_scalar_fields_for_root(&root, 8) {
            if !entry
                .display_fields
                .iter()
                .any(|existing| existing == &field)
            {
                entry.display_fields.push(field);
            }
        }
        if let Some(fields) = schema_registry.object_field_names(type_name) {
            for field in fields {
                let is_scalar = schema_registry
                    .object_field_type(type_name, field)
                    .is_some_and(|ty| schema_registry.object_field_names(ty).is_none());
                if !is_scalar
                    && !entry
                        .relation_fields
                        .iter()
                        .any(|existing| existing == field)
                {
                    entry.relation_fields.push(field.clone());
                }
            }
        }
    }

    let mut families = by_type.into_values().collect::<Vec<_>>();
    for family in &mut families {
        family.lookup_roots.sort();
        family.filter_fields.sort();
        family.filter_fields.dedup();
        family.relation_fields.sort();
        family.relation_fields.dedup();
    }
    families.sort_by(|a, b| a.type_name.cmp(&b.type_name));
    families
}

#[derive(Clone, Debug)]
struct ExactLookupTarget {
    root_field: String,
    arg_name: String,
    matched_field: String,
}

fn schema_candidate_for_family(family: &EntityFamily) -> Option<SchemaEntityCandidate> {
    let mut key_fields = family
        .key_fields
        .iter()
        .filter(|field| {
            family
                .filter_fields
                .iter()
                .any(|candidate| candidate == *field)
        })
        .cloned()
        .collect::<Vec<_>>();
    let mut label_fields = family
        .label_fields
        .iter()
        .filter(|field| {
            family
                .filter_fields
                .iter()
                .any(|candidate| candidate == *field)
        })
        .cloned()
        .collect::<Vec<_>>();
    key_fields.sort();
    key_fields.dedup();
    label_fields.sort();
    label_fields.dedup();
    if family.lookup_roots.is_empty() || (key_fields.is_empty() && label_fields.is_empty()) {
        return None;
    }
    Some(SchemaEntityCandidate {
        family_type: family.type_name.clone(),
        lookup_roots: family.lookup_roots.clone(),
        key_fields,
        label_fields,
        filter_fields: family.filter_fields.clone(),
    })
}

fn schema_resolutions_from_mentions(
    schema_registry: &SchemaRegistry,
    catalog: &[EntityFamily],
    user_message: &str,
    mentions: Vec<String>,
    limit: usize,
) -> Vec<EntityResolution> {
    mentions
        .into_iter()
        .map(|mention| {
            let budget =
                grounding_budget_for_request(schema_registry, user_message, Some(&mention), limit);
            let mut schema_candidates = catalog
                .iter()
                .filter_map(schema_candidate_for_family)
                .collect::<Vec<_>>();
            schema_candidates.sort_by(|a, b| a.family_type.cmp(&b.family_type));
            let (schema_candidates_narrowed, narrowing_note) =
                narrow_schema_candidates_for_mention(
                    schema_registry,
                    schema_candidates,
                    user_message,
                    &mention,
                );
            let mut schema_candidates = schema_candidates_narrowed;
            let total_candidates = schema_candidates.len();
            schema_candidates.truncate(budget.schema_candidate_limit.max(1));
            let mut notes = vec![SCHEMA_CANDIDATE_NOTE.to_string()];
            if let Some(note) = narrowing_note {
                notes.push(note);
            }
            let status = if total_candidates == 0 {
                notes.push(
                    "No schema families expose filterable key/label lookup fields for this mention."
                        .to_string(),
                );
                ResolutionStatus::Unresolved
            } else if total_candidates == 1 {
                ResolutionStatus::SchemaCandidate
            } else {
                if total_candidates > schema_candidates.len() {
                    notes.push(format!(
                        "{} additional schema candidate families omitted from this preview.",
                        total_candidates - schema_candidates.len()
                    ));
                } else {
                    notes.push(
                        "Multiple schema families could support lookup; backend grounding is needed before choosing one."
                            .to_string(),
                    );
                }
                ResolutionStatus::Ambiguous
            };
            EntityResolution {
                mention,
                status,
                grounded_matches: Vec::new(),
                schema_candidates,
                notes,
            }
        })
        .collect()
}

fn normalize_lookup_name(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    lower.strip_suffix('s').unwrap_or(&lower).to_string()
}

fn root_arg_matches_field(arg_name: &str, field: &str) -> bool {
    arg_name.eq_ignore_ascii_case(field)
        || normalize_lookup_name(arg_name) == normalize_lookup_name(field)
}

fn direct_lookup_roots_for_family(family: &EntityFamily) -> Vec<String> {
    let mut get_roots = family
        .lookup_roots
        .iter()
        .filter(|root| root.starts_with("get"))
        .cloned()
        .collect::<Vec<_>>();
    let mut batch_roots = family
        .lookup_roots
        .iter()
        .filter(|root| root.starts_with("batchGet"))
        .cloned()
        .collect::<Vec<_>>();
    get_roots.sort();
    batch_roots.sort();
    get_roots.extend(batch_roots);
    get_roots
}

fn query_lookup_roots_for_family(family: &EntityFamily) -> Vec<String> {
    let mut query_roots = family
        .lookup_roots
        .iter()
        .filter(|root| root.starts_with("query"))
        .cloned()
        .collect::<Vec<_>>();
    query_roots.sort();
    query_roots
}

fn exact_lookup_targets(
    schema_registry: &SchemaRegistry,
    family: &EntityFamily,
) -> Vec<ExactLookupTarget> {
    let mut targets = Vec::new();
    for root_field in direct_lookup_roots_for_family(family) {
        for arg_name in schema_registry.root_arg_names(&root_field) {
            if matches!(arg_name.as_str(), "filter" | "order" | "first" | "offset") {
                continue;
            }
            let Some(matched_field) = family
                .key_fields
                .iter()
                .find(|field| root_arg_matches_field(&arg_name, field))
            else {
                continue;
            };
            targets.push(ExactLookupTarget {
                root_field: root_field.clone(),
                arg_name,
                matched_field: matched_field.clone(),
            });
        }
    }
    targets.sort_by(|a, b| {
        a.root_field
            .cmp(&b.root_field)
            .then_with(|| a.arg_name.cmp(&b.arg_name))
            .then_with(|| a.matched_field.cmp(&b.matched_field))
    });
    targets.dedup_by(|left, right| {
        left.root_field == right.root_field
            && left.arg_name == right.arg_name
            && left.matched_field == right.matched_field
    });
    targets
}

fn family_for_type<'a>(catalog: &'a [EntityFamily], type_name: &str) -> Option<&'a EntityFamily> {
    catalog.iter().find(|family| family.type_name == type_name)
}

fn candidate_families_for_resolution<'a>(
    catalog: &'a [EntityFamily],
    resolution: &EntityResolution,
) -> Vec<&'a EntityFamily> {
    if resolution.schema_candidates.is_empty() {
        return catalog.iter().collect::<Vec<_>>();
    }

    let mut families = resolution
        .schema_candidates
        .iter()
        .filter_map(|candidate| family_for_type(catalog, &candidate.family_type))
        .collect::<Vec<_>>();
    families.sort_by(|left, right| left.type_name.cmp(&right.type_name));
    families.dedup_by(|left, right| left.type_name == right.type_name);
    families
}

fn exact_scoped_query_filter_fields(
    schema_registry: &SchemaRegistry,
    family: &EntityFamily,
    root_field: &str,
    prefer_key_fields: bool,
) -> Vec<String> {
    let filter_fields = schema_registry.root_filter_fields(root_field);
    let mut out = Vec::new();
    let mut push_if_supported = |field: &str| {
        if out.iter().any(|existing| existing == field) {
            return;
        }
        if let Some(supported) = filter_fields
            .iter()
            .find(|candidate| candidate.eq_ignore_ascii_case(field))
        {
            out.push(supported.clone());
        }
    };

    let first = if prefer_key_fields {
        [&family.key_fields, &family.label_fields]
    } else {
        [&family.label_fields, &family.key_fields]
    };
    for fields in first {
        for field in fields {
            push_if_supported(field);
        }
    }
    out
}

async fn exact_scoped_query_grounding_for_resolution(
    state: &AppState,
    schema_registry: &SchemaRegistry,
    candidate_families: &[&EntityFamily],
    resolution: &EntityResolution,
    prefer_key_fields: bool,
    match_limit: usize,
) -> (usize, usize, Vec<GroundedEntityMatch>) {
    let variants = if looks_like_compact_identifier(&resolution.mention) {
        exact_identifier_lookup_variants(&resolution.mention)
    } else {
        exact_label_lookup_variants(&resolution.mention)
    };
    if variants.is_empty() {
        return (0, 0, Vec::new());
    }

    let (bearer_token, api_key_header, api_key) = auth_parts(state);
    let mut attempted = 0usize;
    let mut failed = 0usize;
    let mut grounded_matches = Vec::new();

    for family in candidate_families {
        for root_field in query_lookup_roots_for_family(family) {
            let filter_fields = exact_scoped_query_filter_fields(
                schema_registry,
                family,
                &root_field,
                prefer_key_fields,
            );
            if filter_fields.is_empty() {
                continue;
            }

            for field in filter_fields {
                for variant in &variants {
                    let Some(filter) =
                        build_exact_filter(schema_registry, &root_field, &field, variant)
                    else {
                        continue;
                    };
                    let fields =
                        lookup_selection_fields(schema_registry, family, &root_field, &field);
                    let Some(query) = ir_to_graphql(&IRQuery {
                        root_field: root_field.clone(),
                        fields,
                        first: Some(2),
                        offset: None,
                        filter: Some(filter),
                        order: None,
                    }) else {
                        continue;
                    };
                    attempted += 1;
                    let response = match execute_graphql(
                        &state.client,
                        &state.config.graph.graph_endpoint,
                        bearer_token,
                        api_key_header,
                        api_key,
                        &query,
                        &serde_json::json!({}),
                    )
                    .await
                    {
                        Ok(response) => response,
                        Err(_) => {
                            failed += 1;
                            continue;
                        }
                    };
                    if response
                        .get("errors")
                        .and_then(|value| value.as_array())
                        .is_some_and(|errors| !errors.is_empty())
                    {
                        failed += 1;
                        continue;
                    }
                    for row in rows_from_lookup_response(&response, &root_field) {
                        let Some(grounded) = grounded_match_from_row(
                            schema_registry,
                            &row,
                            family,
                            &root_field,
                            &field,
                            &resolution.mention,
                        ) else {
                            continue;
                        };
                        insert_grounded_match(&mut grounded_matches, grounded);
                        if grounded_matches.len() >= match_limit {
                            return (attempted, failed, grounded_matches);
                        }
                    }
                }
            }
        }
    }

    (attempted, failed, grounded_matches)
}

fn collapse_field_context_ambiguity(
    schema_registry: &SchemaRegistry,
    user_message: &str,
    resolutions: &mut [EntityResolution],
) {
    let lower_message = user_message.to_ascii_lowercase();
    let comparison_like = message_mentions_vocabulary_terms(
        user_message,
        &schema_registry.intent_vocabulary().compare,
    );

    let mut contextual_fields = BTreeSet::new();
    for resolution in resolutions.iter() {
        if resolution.status != ResolutionStatus::Grounded || resolution.grounded_matches.len() != 1
        {
            continue;
        }
        let grounded = &resolution.grounded_matches[0];
        if looks_like_compact_identifier(&resolution.mention) {
            continue;
        }
        contextual_fields.insert((
            grounded.family_type.clone(),
            grounded.root_field.clone(),
            grounded.matched_field.clone(),
        ));
    }

    for resolution in resolutions.iter_mut() {
        if resolution.status != ResolutionStatus::Ambiguous || resolution.grounded_matches.len() < 2
        {
            continue;
        }

        let stable_entity_matches = resolution
            .grounded_matches
            .iter()
            .filter(|grounded| {
                schema_registry
                    .root_time_filter_fields(&grounded.root_field)
                    .is_empty()
            })
            .cloned()
            .collect::<Vec<_>>();
        if stable_entity_matches.len() == 1
            && resolution.grounded_matches.iter().all(|grounded| {
                grounded
                    .matched_value
                    .eq_ignore_ascii_case(&stable_entity_matches[0].matched_value)
                    || grounded
                        .mention
                        .eq_ignore_ascii_case(&stable_entity_matches[0].matched_value)
            })
        {
            resolution.grounded_matches = stable_entity_matches;
            resolution.status = ResolutionStatus::Grounded;
            resolution.notes.push(
                "Ambiguous grounding collapsed to the stable entity record instead of time-series observations."
                    .to_string(),
            );
            continue;
        }

        let explicit_field_matches = resolution
            .grounded_matches
            .iter()
            .filter(|grounded| lower_message.contains(&grounded.matched_field.to_ascii_lowercase()))
            .cloned()
            .collect::<Vec<_>>();
        let preferred_matches = if !explicit_field_matches.is_empty() {
            explicit_field_matches
        } else if comparison_like {
            resolution
                .grounded_matches
                .iter()
                .filter(|grounded| {
                    contextual_fields.contains(&(
                        grounded.family_type.clone(),
                        grounded.root_field.clone(),
                        grounded.matched_field.clone(),
                    ))
                })
                .cloned()
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };

        let mut preferred_fields = preferred_matches
            .iter()
            .map(|grounded| grounded.matched_field.clone())
            .collect::<Vec<_>>();
        preferred_fields.sort();
        preferred_fields.dedup();
        if preferred_matches.len() == 1 || preferred_fields.len() == 1 {
            resolution.grounded_matches = preferred_matches
                .into_iter()
                .take(1)
                .collect::<Vec<GroundedEntityMatch>>();
            resolution.status = ResolutionStatus::Grounded;
            resolution.notes.push(
                "Ambiguous grounding collapsed to the field implied by the request context."
                    .to_string(),
            );
        }
    }
}

fn push_label_grounding_root(
    roots: &mut Vec<String>,
    schema_registry: &SchemaRegistry,
    catalog: &[EntityFamily],
    root_field: &str,
) {
    if roots.iter().any(|existing| existing == root_field) {
        return;
    }
    let Some(return_type) = schema_registry.query_return_type(root_field) else {
        return;
    };
    let Some(family) = family_for_type(catalog, return_type) else {
        return;
    };
    let filter_fields = schema_registry.root_filter_fields(root_field);
    let has_label_filter = family.label_fields.iter().any(|field| {
        filter_fields
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(field))
    });
    if has_label_filter {
        roots.push(root_field.to_string());
    }
}

fn label_grounding_roots(
    schema_registry: &SchemaRegistry,
    catalog: &[EntityFamily],
    user_message: &str,
    budget: GroundingBudget,
    resolution: Option<&EntityResolution>,
) -> Vec<String> {
    let retrieval = schema_registry.query_root_retrieval_profile(user_message, 8);
    let prioritized_limit = match retrieval.confidence {
        RetrievalConfidence::High => budget.prioritized_root_limit.saturating_sub(2).max(3),
        RetrievalConfidence::Medium => budget.prioritized_root_limit,
        RetrievalConfidence::Low => (budget.prioritized_root_limit + 2).min(12),
    };
    let fallback_reserve = if resolution.is_some() { 2 } else { 1 };
    let prioritized_take_limit = prioritized_limit
        .min(
            budget
                .root_guided_label_root_limit
                .saturating_sub(fallback_reserve)
                .max(1),
        )
        .max(1);
    let top_score = retrieval.top_score;
    let prioritized = retrieval
        .matches
        .into_iter()
        .filter(|entry| top_score == 0 || entry.score >= (top_score - 40).max(1))
        .take(prioritized_take_limit)
        .map(|entry| entry.root)
        .collect::<Vec<_>>();
    let mut roots = Vec::new();

    for root_field in prioritized {
        push_label_grounding_root(&mut roots, schema_registry, catalog, &root_field);
    }

    if let Some(resolution) = resolution {
        let mut candidate_family_roots = resolution
            .schema_candidates
            .iter()
            .flat_map(|candidate| candidate.lookup_roots.iter().cloned())
            .collect::<Vec<_>>();
        candidate_family_roots.sort();
        candidate_family_roots.dedup();
        for root_field in candidate_family_roots {
            push_label_grounding_root(&mut roots, schema_registry, catalog, &root_field);
            if roots.len() >= budget.root_guided_label_root_limit {
                break;
            }
        }
    }

    let mut schema_supported = catalog
        .iter()
        .flat_map(|family| family.lookup_roots.iter().cloned())
        .collect::<Vec<_>>();
    schema_supported.sort();
    schema_supported.dedup();
    for root_field in schema_supported {
        push_label_grounding_root(&mut roots, schema_registry, catalog, &root_field);
        if roots.len() >= budget.root_guided_label_root_limit {
            break;
        }
    }

    roots
}

fn typed_root_arg_value(type_ref: &InputTypeRef, raw_value: &str) -> Value {
    if type_ref.is_list {
        Value::Array(vec![typed_lookup_json_value(&type_ref.name, raw_value)])
    } else {
        typed_lookup_json_value(&type_ref.name, raw_value)
    }
}

fn build_direct_lookup_query(
    root_field: &str,
    arg_name: &str,
    arg_value: &Value,
    selection_fields: &[String],
) -> Option<String> {
    if selection_fields.is_empty() {
        return None;
    }
    let rendered_fields = selection_fields
        .iter()
        .map(|field| format!("    {field}"))
        .collect::<Vec<_>>()
        .join("\n");
    Some(format!(
        "query EntityLookup {{\n  {root_field}({arg_name}: {}) {{\n{rendered_fields}\n  }}\n}}",
        graphql_value(arg_value)
    ))
}

async fn exact_backend_grounding_for_resolution(
    state: &AppState,
    schema_registry: &SchemaRegistry,
    catalog: &[EntityFamily],
    user_message: &str,
    base_limit: usize,
    resolution: &mut EntityResolution,
) {
    clear_schema_only_grounding_notes(&mut resolution.notes);
    let budget =
        grounding_budget_for_resolution(schema_registry, user_message, resolution, base_limit);

    if !looks_like_compact_identifier(&resolution.mention) {
        resolution
            .notes
            .push("Backend grounding skipped for non-identifier mention.".to_string());
        return;
    }
    if state.config.graph.graph_endpoint.trim().is_empty() {
        resolution.notes.push(
            "Backend grounding skipped because no GraphQL endpoint is configured.".to_string(),
        );
        return;
    }

    let mut attempted_lookups = 0usize;
    let mut failed_lookups = 0usize;
    let mut grounded_matches = Vec::new();
    let (bearer_token, api_key_header, api_key) = auth_parts(state);
    let candidate_families = candidate_families_for_resolution(catalog, resolution);

    'families: for family in &candidate_families {
        for target in exact_lookup_targets(schema_registry, family) {
            let Some(type_ref) =
                schema_registry.root_arg_type_ref(&target.root_field, &target.arg_name)
            else {
                continue;
            };
            let arg_value = typed_root_arg_value(&type_ref, &resolution.mention);
            let selection_fields = lookup_selection_fields(
                schema_registry,
                family,
                &target.root_field,
                &target.matched_field,
            );
            let Some(query) = build_direct_lookup_query(
                &target.root_field,
                &target.arg_name,
                &arg_value,
                &selection_fields,
            ) else {
                continue;
            };
            attempted_lookups += 1;
            let response = match execute_graphql(
                &state.client,
                &state.config.graph.graph_endpoint,
                bearer_token,
                api_key_header,
                api_key,
                &query,
                &serde_json::json!({}),
            )
            .await
            {
                Ok(response) => response,
                Err(_) => {
                    failed_lookups += 1;
                    continue;
                }
            };
            if response
                .get("errors")
                .and_then(|value| value.as_array())
                .is_some_and(|errors| !errors.is_empty())
            {
                failed_lookups += 1;
                continue;
            }
            for row in rows_from_lookup_response(&response, &target.root_field) {
                let Some(grounded) = grounded_match_from_row(
                    schema_registry,
                    &row,
                    family,
                    &target.root_field,
                    &target.matched_field,
                    &resolution.mention,
                ) else {
                    continue;
                };
                insert_grounded_match(&mut grounded_matches, grounded);
                if grounded_matches.len() >= budget.grounded_match_limit {
                    break 'families;
                }
            }
        }
    }

    resolution.grounded_matches = grounded_matches;
    if resolution.grounded_matches.len() == 1 {
        resolution.status = ResolutionStatus::Grounded;
        resolution.notes.push(
            "Grounded via exact backend lookup on a schema-defined root argument.".to_string(),
        );
        return;
    }
    if resolution.grounded_matches.len() > 1 {
        resolution.status = ResolutionStatus::Ambiguous;
        resolution.notes.push(
            "Multiple exact backend matches were found; clarification may be required.".to_string(),
        );
        return;
    }

    let no_exact_lookup_targets = attempted_lookups == 0;
    if no_exact_lookup_targets {
        resolution.notes.push(
            "No exact schema-defined lookup roots were available for backend grounding."
                .to_string(),
        );
    }

    if failed_lookups > 0 {
        resolution.notes.push(format!(
            "{failed_lookups} exact backend lookup attempts failed while grounding this mention."
        ));
    }

    let (scoped_attempted, scoped_failed, scoped_matches) =
        exact_scoped_query_grounding_for_resolution(
            state,
            schema_registry,
            &candidate_families,
            resolution,
            true,
            budget.grounded_match_limit,
        )
        .await;
    resolution.grounded_matches = scoped_matches;
    if resolution.grounded_matches.len() == 1 {
        resolution.status = ResolutionStatus::Grounded;
        resolution.notes.push(
            "Grounded via exact scoped query lookup on the narrowed entity family.".to_string(),
        );
        return;
    }
    if resolution.grounded_matches.len() > 1 {
        resolution.status = ResolutionStatus::Ambiguous;
        resolution.notes.push(
            "Multiple exact scoped query matches were found; clarification may be required."
                .to_string(),
        );
        return;
    }
    if scoped_failed > 0 {
        resolution.notes.push(format!(
            "{scoped_failed} exact scoped query grounding lookup attempts failed."
        ));
    }
    if no_exact_lookup_targets && scoped_attempted == 0 {
        return;
    }

    resolution.status = ResolutionStatus::Unresolved;
    resolution.notes.push(
        "No exact backend match was found for this compact identifier-style mention.".to_string(),
    );
}

async fn root_guided_label_grounding_for_resolution(
    state: &AppState,
    schema_registry: &SchemaRegistry,
    catalog: &[EntityFamily],
    user_message: &str,
    base_limit: usize,
    resolution: &mut EntityResolution,
) {
    clear_schema_only_grounding_notes(&mut resolution.notes);
    let budget =
        grounding_budget_for_resolution(schema_registry, user_message, resolution, base_limit);

    if state.config.graph.graph_endpoint.trim().is_empty() {
        resolution.notes.push(
            "Backend grounding skipped because no GraphQL endpoint is configured.".to_string(),
        );
        return;
    }

    let candidate_roots = label_grounding_roots(
        schema_registry,
        catalog,
        user_message,
        budget,
        Some(resolution),
    );
    if candidate_roots.is_empty() {
        resolution.notes.push(
            "No schema-supported label lookup roots were available for backend grounding."
                .to_string(),
        );
        return;
    }

    let mut attempted = 0usize;
    let mut failed = 0usize;
    let mut grounded_matches = Vec::new();
    let (bearer_token, api_key_header, api_key) = auth_parts(state);

    for root_field in candidate_roots {
        let Some(return_type) = schema_registry.query_return_type(&root_field) else {
            continue;
        };
        let Some(family) = family_for_type(catalog, return_type) else {
            continue;
        };
        let filter_fields = schema_registry.root_filter_fields(&root_field);
        let label_fields = family
            .label_fields
            .iter()
            .filter_map(|field| {
                filter_fields
                    .iter()
                    .find(|candidate| candidate.eq_ignore_ascii_case(field))
                    .cloned()
            })
            .collect::<Vec<_>>();
        if label_fields.is_empty() {
            continue;
        }

        for field in label_fields {
            for variant in exact_label_lookup_variants(&resolution.mention) {
                let Some(filter) =
                    build_exact_filter(schema_registry, &root_field, &field, &variant)
                else {
                    continue;
                };
                let fields = lookup_selection_fields(schema_registry, family, &root_field, &field);
                let Some(query) = ir_to_graphql(&IRQuery {
                    root_field: root_field.clone(),
                    fields,
                    first: Some(2),
                    offset: None,
                    filter: Some(filter),
                    order: None,
                }) else {
                    continue;
                };
                attempted += 1;
                let response = match execute_graphql(
                    &state.client,
                    &state.config.graph.graph_endpoint,
                    bearer_token,
                    api_key_header,
                    api_key,
                    &query,
                    &serde_json::json!({}),
                )
                .await
                {
                    Ok(response) => response,
                    Err(_) => {
                        failed += 1;
                        continue;
                    }
                };
                if response
                    .get("errors")
                    .and_then(|value| value.as_array())
                    .is_some_and(|errors| !errors.is_empty())
                {
                    failed += 1;
                    continue;
                }
                for row in rows_from_lookup_response(&response, &root_field) {
                    let Some(grounded) = grounded_match_from_row(
                        schema_registry,
                        &row,
                        family,
                        &root_field,
                        &field,
                        &resolution.mention,
                    ) else {
                        continue;
                    };
                    insert_grounded_match(&mut grounded_matches, grounded);
                    if grounded_matches.len() >= budget.grounded_match_limit {
                        break;
                    }
                }
                if grounded_matches.len() >= budget.grounded_match_limit {
                    break;
                }
            }
            if grounded_matches.len() >= budget.grounded_match_limit {
                break;
            }
        }
        if grounded_matches.len() >= budget.grounded_match_limit {
            break;
        }
    }

    if grounded_matches.is_empty() {
        if attempted == 0 {
            resolution.notes.push(
                "No exact label filters were available on schema-supported lookup roots for backend grounding."
                    .to_string(),
            );
        } else if failed > 0 {
            resolution.notes.push(format!(
                "{failed} root-guided label grounding lookup attempts failed."
            ));
        }

        let candidate_families = candidate_families_for_resolution(catalog, resolution);
        let (scoped_attempted, scoped_failed, scoped_matches) =
            exact_scoped_query_grounding_for_resolution(
                state,
                schema_registry,
                &candidate_families,
                resolution,
                false,
                budget.grounded_match_limit,
            )
            .await;
        attempted += scoped_attempted;
        if !scoped_matches.is_empty() {
            resolution.grounded_matches = scoped_matches;
            if resolution.grounded_matches.len() == 1 {
                descriptive_grounding_requires_confirmation(
                    resolution,
                    "Backend found one exact scoped query candidate on the narrowed entity family; user confirmation is required before execution.",
                );
            } else {
                resolution.status = ResolutionStatus::Ambiguous;
                resolution.notes.push(
                    "Multiple exact scoped query matches were found; clarification may be required."
                        .to_string(),
                );
            }
            return;
        }
        if scoped_failed > 0 {
            resolution.notes.push(format!(
                "{scoped_failed} exact scoped query grounding lookup attempts failed."
            ));
        }

        if attempted == 0 {
            resolution.notes.push(
                "No exact label filters were available on schema-supported lookup roots for backend grounding."
                    .to_string(),
            );
        } else {
            resolution.notes.push(
                "No exact label match was found on the schema-supported roots probed for this mention."
                    .to_string(),
            );
        }
        return;
    }

    resolution.grounded_matches = grounded_matches;
    if resolution.grounded_matches.len() == 1 {
        descriptive_grounding_requires_confirmation(
            resolution,
            "Backend found one exact label candidate on schema-supported roots; user confirmation is required before execution.",
        );
    } else {
        resolution.status = ResolutionStatus::Ambiguous;
        resolution.notes.push(
            "Multiple exact label matches were found on schema-supported lookup roots; clarification may be required."
                .to_string(),
        );
    }
}

pub(crate) fn resolve_entity_resolutions(
    schema_registry: &SchemaRegistry,
    user_message: &str,
    limit: usize,
) -> Vec<EntityResolution> {
    let catalog = build_entity_catalog(schema_registry);
    let mentions = extract_generic_mentions(schema_registry, &catalog, user_message);
    if mentions.is_empty() {
        return Vec::new();
    }
    let mut resolutions =
        schema_resolutions_from_mentions(schema_registry, &catalog, user_message, mentions, limit);
    for resolution in &mut resolutions {
        resolution
            .notes
            .push(SCHEMA_ONLY_GROUNDING_NOTE.to_string());
    }
    resolutions
}

pub(crate) async fn resolve_grounded_entity_resolutions(
    state: &AppState,
    schema_registry: &SchemaRegistry,
    user_message: &str,
    limit: usize,
) -> Vec<EntityResolution> {
    let schema_version = state.schema_meta.read().await.loaded_at.to_rfc3339();
    let question_cache_key = GroundingQuestionCacheKey::new(
        schema_version.clone(),
        normalize_grounding_question(user_message),
        limit,
    );
    if let Some(mut cached) = state
        .planner_cache
        .write()
        .await
        .grounding_question_entry(&question_cache_key)
    {
        collapse_field_context_ambiguity(schema_registry, user_message, &mut cached);
        for resolution in &mut cached {
            resolution
                .notes
                .push("Grounding reused from normalized-question cache.".to_string());
        }
        return cached;
    }

    let catalog = build_entity_catalog(schema_registry);
    let mentions = extract_generic_mentions(schema_registry, &catalog, user_message);
    if mentions.is_empty() {
        state
            .planner_cache
            .write()
            .await
            .insert_grounding_question_entry(question_cache_key, Vec::new());
        return Vec::new();
    }
    let mut resolutions =
        schema_resolutions_from_mentions(schema_registry, &catalog, user_message, mentions, limit);
    for resolution in &mut resolutions {
        clear_schema_only_grounding_notes(&mut resolution.notes);
        if looks_like_compact_identifier(&resolution.mention) {
            let cache_key =
                GroundingCacheKey::new(schema_version.clone(), &resolution.mention, true, limit);
            if let Some(cached) = state.planner_cache.read().await.grounding_entry(&cache_key) {
                *resolution = cached;
                resolution
                    .notes
                    .push("Grounding reused from in-memory cache.".to_string());
            } else {
                exact_backend_grounding_for_resolution(
                    state,
                    schema_registry,
                    &catalog,
                    user_message,
                    limit,
                    resolution,
                )
                .await;
                state
                    .planner_cache
                    .write()
                    .await
                    .insert_grounding_entry(cache_key, resolution.clone());
            }
        } else {
            let cache_key =
                GroundingCacheKey::new(schema_version.clone(), &resolution.mention, false, limit);
            if let Some(cached) = state.planner_cache.read().await.grounding_entry(&cache_key) {
                *resolution = cached;
                resolution
                    .notes
                    .push("Grounding reused from in-memory cache.".to_string());
            } else {
                root_guided_label_grounding_for_resolution(
                    state,
                    schema_registry,
                    &catalog,
                    user_message,
                    limit,
                    resolution,
                )
                .await;
                state
                    .planner_cache
                    .write()
                    .await
                    .insert_grounding_entry(cache_key, resolution.clone());
            }
        }
    }
    collapse_field_context_ambiguity(schema_registry, user_message, &mut resolutions);
    state
        .planner_cache
        .write()
        .await
        .insert_grounding_question_entry(question_cache_key, resolutions.clone());
    resolutions
}

fn normalize_grounding_question(user_message: &str) -> String {
    user_message
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

pub(crate) fn render_entity_resolution_block_from_resolutions(
    resolutions: &[EntityResolution],
) -> String {
    if resolutions.is_empty() {
        return "Entity resolution: none".to_string();
    }

    let has_grounded = resolutions
        .iter()
        .any(|resolution| !resolution.grounded_matches.is_empty());
    let mut lines = vec![if has_grounded {
        "Entity resolution:".to_string()
    } else {
        "Entity resolution (schema-derived candidates only):".to_string()
    }];
    for resolution in resolutions {
        lines.push(format!(
            "- mention=`{}` status=`{}`",
            resolution.mention,
            resolution.status.as_str()
        ));
        for grounded in &resolution.grounded_matches {
            lines.push(format!(
                "  grounded: type=`{}` root=`{}` field=`{}` matched_value=`{}`{}{}",
                grounded.family_type,
                grounded.root_field,
                grounded.matched_field,
                grounded.matched_value,
                grounded
                    .stable_key_field
                    .as_ref()
                    .zip(grounded.stable_key_value.as_ref())
                    .map(|(field, value)| format!(" stable_key=`{field}:{value}`"))
                    .unwrap_or_default(),
                grounded
                    .display_label
                    .as_ref()
                    .map(|label| format!(" label=`{label}`"))
                    .unwrap_or_default()
            ));
        }
        for candidate in &resolution.schema_candidates {
            let roots = if candidate.lookup_roots.is_empty() {
                "none".to_string()
            } else {
                candidate.lookup_roots.join(", ")
            };
            let key_fields = if candidate.key_fields.is_empty() {
                "none".to_string()
            } else {
                candidate.key_fields.join(", ")
            };
            let label_fields = if candidate.label_fields.is_empty() {
                "none".to_string()
            } else {
                candidate.label_fields.join(", ")
            };
            lines.push(format!(
                "  candidate: type=`{}` roots=`{}` key_fields=`{}` label_fields=`{}`",
                candidate.family_type, roots, key_fields, label_fields
            ));
        }
        for note in &resolution.notes {
            lines.push(format!("  note: {note}"));
        }
    }
    lines.join("\n")
}

#[cfg(test)]
pub(crate) fn render_entity_resolution_block(
    schema_registry: &SchemaRegistry,
    user_message: &str,
    limit: usize,
) -> String {
    let resolutions = resolve_entity_resolutions(schema_registry, user_message, limit);
    render_entity_resolution_block_from_resolutions(&resolutions)
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_GROUNDED_MATCHES_PER_MENTION, DEFAULT_ROOT_GUIDED_LABEL_ROOTS, EntityResolution,
        GroundedEntityMatch, ResolutionStatus, SCHEMA_CANDIDATE_NOTE, SCHEMA_ONLY_GROUNDING_NOTE,
        SchemaEntityCandidate, build_entity_catalog, clear_schema_only_grounding_notes,
        collapse_field_context_ambiguity, exact_identifier_lookup_variants,
        extracted_entity_mentions, grounded_match_from_row, grounding_budget_for_request,
        grounding_budget_for_resolution, insert_grounded_match, label_grounding_roots,
        looks_like_compact_identifier, lookup_selection_fields, render_entity_resolution_block,
        render_entity_resolution_block_from_resolutions, resolve_entity_resolutions,
        schema_resolutions_from_mentions,
    };
    use crate::schema_registry::SchemaRegistry;
    use crate::sls::{Sls, load_sls_merged};
    use serde_json::json;

    fn registry() -> SchemaRegistry {
        SchemaRegistry::new(include_str!("../schemas/consumer_schema.graphql"))
    }

    fn registry_with_default_sls() -> SchemaRegistry {
        let sls = test_sls();
        registry_with_sls(&sls)
    }

    fn registry_with_sls(sls: &Sls) -> SchemaRegistry {
        SchemaRegistry::with_sls(
            include_str!("../schemas/consumer_schema.graphql"),
            Some(sls),
        )
    }

    fn test_sls() -> Sls {
        let schema = include_str!("../schemas/consumer_schema.graphql");
        let bootstrap = SchemaRegistry::new(schema);
        load_sls_merged(&bootstrap, "sls.yaml").expect("load sls")
    }

    #[test]
    fn schema_entity_resolution_is_explicitly_non_grounded_for_quoted_label() {
        let schema = registry();
        let resolutions =
            resolve_entity_resolutions(&schema, r#"Show details for "Wind Farm 1"."#, 8);
        let resolution = resolutions
            .iter()
            .find(|resolution| resolution.mention == "Wind Farm 1")
            .expect("expected Wind Farm 1 resolution");
        assert!(
            resolution.status != ResolutionStatus::Grounded,
            "schema-only resolver should not claim grounding"
        );
        assert!(
            !resolution.schema_candidates.is_empty(),
            "expected schema candidates for plain-name entity"
        );
        assert!(
            resolution
                .notes
                .iter()
                .any(|note| note == SCHEMA_ONLY_GROUNDING_NOTE),
            "expected non-grounded note, got {:?}",
            resolution.notes
        );
    }

    #[test]
    fn mention_extraction_skips_rank_phrases() {
        let schema = registry_with_default_sls();
        let mentions = extracted_entity_mentions(&schema, "Top 3 wind farms by ratedCapacity.");
        assert!(
            !mentions.iter().any(|mention| mention == "Top 3"),
            "did not expect Top 3 to be treated as an entity mention: {mentions:?}"
        );
    }

    #[test]
    fn mention_extraction_does_not_infer_numbered_labels_from_entity_cues() {
        let schema = registry_with_default_sls();
        let mentions = extracted_entity_mentions(
            &schema,
            "Compare ratedCapacity between Wind Farm 1 and Wind Farm 2.",
        );
        assert!(
            !mentions.iter().any(|mention| mention == "Wind Farm 1")
                && !mentions.iter().any(|mention| mention == "Wind Farm 2"),
            "numbered labels should not be inferred from entity phrase shape alone: {mentions:?}"
        );

        let ranked_mentions =
            extracted_entity_mentions(&schema, "Top 3 wind farms by ratedCapacity.");
        assert!(
            !ranked_mentions
                .iter()
                .any(|mention| mention.eq_ignore_ascii_case("wind farm 3")),
            "did not expect rank count to become a wind farm mention: {ranked_mentions:?}"
        );
    }

    #[test]
    fn mention_extraction_uses_schema_cued_lowercase_labels() {
        let schema = registry_with_default_sls();
        let mentions =
            extracted_entity_mentions(&schema, "Show details for wind farm named dudgeon.");
        assert!(
            mentions.iter().any(|mention| mention == "dudgeon"),
            "expected lowercase label after explicit schema-derived label cue: {mentions:?}"
        );

        let compared_mentions = extracted_entity_mentions(
            &schema,
            "Compare wind farm named dudgeon and wind farm named hywind.",
        );
        assert!(
            compared_mentions.iter().any(|mention| mention == "dudgeon"),
            "expected first lowercase label after explicit label cue: {compared_mentions:?}"
        );
        assert!(
            compared_mentions.iter().any(|mention| mention == "hywind"),
            "expected second lowercase label after explicit label cue: {compared_mentions:?}"
        );
    }

    #[test]
    fn mention_extraction_uses_sls_label_cues_not_builtin_words() {
        let mut sls = test_sls();
        sls.intent_vocabulary.label_cues = vec!["tagged".to_string()];
        let schema = registry_with_sls(&sls);

        let tagged_mentions =
            extracted_entity_mentions(&schema, "Show details for wind farm tagged dudgeon.");
        assert!(
            tagged_mentions.iter().any(|mention| mention == "dudgeon"),
            "expected custom SLS label cue to extract lowercase label: {tagged_mentions:?}"
        );

        let named_mentions =
            extracted_entity_mentions(&schema, "Show details for wind farm named dudgeon.");
        assert!(
            !named_mentions.iter().any(|mention| mention == "dudgeon"),
            "did not expect removed label cue to keep working as a Rust literal: {named_mentions:?}"
        );
    }

    #[test]
    fn mention_extraction_uses_sls_entity_connectors_to_split_labels() {
        let mut sls = test_sls();
        sls.intent_vocabulary.label_cues = vec!["named".to_string()];
        sls.intent_vocabulary.entity_connectors = vec!["plus".to_string()];
        let schema = registry_with_sls(&sls);

        let mentions = extracted_entity_mentions(
            &schema,
            "Compare wind farm named dudgeon plus wind farm named hywind.",
        );
        assert!(
            mentions.iter().any(|mention| mention == "dudgeon"),
            "expected first label to stop at custom SLS connector: {mentions:?}"
        );
        assert!(
            mentions.iter().any(|mention| mention == "hywind"),
            "expected second label after custom SLS connector: {mentions:?}"
        );
    }

    #[test]
    fn mention_extraction_trims_schema_cued_non_entity_tail_words() {
        let schema = registry_with_default_sls();
        let turbine_mentions = extracted_entity_mentions(&schema, "Show turbine T3 details.");
        assert!(
            turbine_mentions
                .iter()
                .any(|mention| mention.eq_ignore_ascii_case("T3")),
            "expected detail suffix to be trimmed from compact turbine mention: {turbine_mentions:?}"
        );
        assert!(
            !turbine_mentions
                .iter()
                .any(|mention| mention.eq_ignore_ascii_case("T3 details")),
            "did not expect detail suffix to become part of mention: {turbine_mentions:?}"
        );

        let ranked_mentions =
            extracted_entity_mentions(&schema, "Which wind farm has the highest ratedCapacity?");
        assert!(
            !ranked_mentions
                .iter()
                .any(|mention| mention.eq_ignore_ascii_case("has the")),
            "did not expect ranking phrase to become a mention: {ranked_mentions:?}"
        );

        let category_mentions = extracted_entity_mentions(
            &schema,
            "Top 5 tag categories by count for plantId PLANT-  4.",
        );
        assert!(
            !category_mentions
                .iter()
                .any(|mention| mention.eq_ignore_ascii_case("categories")),
            "did not expect plural category noun to become an entity mention: {category_mentions:?}"
        );

        let closest_mentions =
            extracted_entity_mentions(&schema, r#"What's the closest turbine to "the wagon"?"#);
        assert!(
            closest_mentions
                .iter()
                .any(|mention| mention == "the wagon"),
            "expected quoted vessel label to be extracted: {closest_mentions:?}"
        );
        assert!(
            !closest_mentions
                .iter()
                .any(|mention| mention.eq_ignore_ascii_case("to")),
            "did not expect relation preposition to become an entity mention: {closest_mentions:?}"
        );
    }

    #[test]
    fn mention_extraction_uses_schema_derived_field_cues() {
        let schema = registry_with_default_sls();
        let mentions =
            extracted_entity_mentions(&schema, "Show details for wind farm with shortName WF3.");
        assert!(
            mentions.iter().any(|mention| mention == "WF3"),
            "expected WF3 to be extracted from schema-derived shortName cue: {mentions:?}"
        );
    }

    #[test]
    fn schema_resolution_uses_wind_farm_type_hint() {
        let schema = registry_with_default_sls();
        let resolutions =
            resolve_entity_resolutions(&schema, "Show details for wind farm named Wind Farm 1.", 8);
        let resolution = resolutions
            .iter()
            .find(|resolution| resolution.mention == "Wind Farm 1")
            .expect("expected Wind Farm 1 resolution");
        assert_eq!(
            resolution.status,
            ResolutionStatus::SchemaCandidate,
            "expected type hint to collapse ambiguity: {resolution:?}"
        );
        assert_eq!(resolution.schema_candidates.len(), 1);
        assert_eq!(
            resolution.schema_candidates[0].family_type,
            "OffshoreWindFarm"
        );
    }

    #[test]
    fn schema_resolution_does_not_apply_target_type_to_separate_label_mention() {
        let schema = registry();
        let resolutions =
            resolve_entity_resolutions(&schema, r#"What's the closest turbine to "the wagon"?"#, 8);
        let resolution = resolutions
            .iter()
            .find(|resolution| resolution.mention == "the wagon")
            .expect("expected the wagon resolution");

        assert!(
            !resolution
                .schema_candidates
                .iter()
                .any(|candidate| candidate.family_type == "OffshoreWindTurbine"),
            "did not expect target type `turbine` to narrow the separate vessel label mention: {resolution:?}"
        );
    }

    #[test]
    fn schema_resolution_uses_shortname_prefix_hint() {
        let schema = registry();
        let resolutions =
            resolve_entity_resolutions(&schema, "Show details for shortName \"WF3\".", 8);
        let resolution = resolutions
            .iter()
            .find(|resolution| resolution.mention == "WF3")
            .expect("expected WF3 resolution");
        assert!(
            resolution
                .schema_candidates
                .iter()
                .all(|candidate| candidate.family_type == "OffshoreWindFarm"),
            "expected WF3 to narrow to OffshoreWindFarm candidates: {resolution:?}"
        );
    }

    #[test]
    fn schema_resolution_uses_tag_field_cues_without_family_table() {
        let schema = registry();
        let resolutions =
            resolve_entity_resolutions(&schema, "List top tag category for plantId PLANT-004.", 8);
        let resolution = resolutions
            .iter()
            .find(|resolution| resolution.mention == "PLANT-004")
            .expect("expected PLANT-004 resolution");
        assert!(
            resolution
                .schema_candidates
                .iter()
                .all(|candidate| candidate.family_type == "Tag"),
            "expected PLANT-004 to narrow to Tag via schema field cues: {resolution:?}"
        );
    }

    #[test]
    fn exact_identifier_lookup_variants_include_dezeroed_space_padded_forms() {
        let variants = exact_identifier_lookup_variants("OSS-003");
        assert!(
            variants.iter().any(|variant| variant == "OSS-3")
                && variants.iter().any(|variant| variant == "OSS- 3")
                && variants.iter().any(|variant| variant == "OSS-  3")
                && variants.iter().any(|variant| variant == "OSS-003"),
            "expected OSS variants to include compact, space-padded, and zero-padded forms: {variants:?}"
        );

        let plant_variants = exact_identifier_lookup_variants("PLANT-004");
        assert!(
            plant_variants.iter().any(|variant| variant == "PLANT-4")
                && plant_variants.iter().any(|variant| variant == "PLANT- 4")
                && plant_variants.iter().any(|variant| variant == "PLANT-  4"),
            "expected PLANT variants to include de-zeroed space-padded forms: {plant_variants:?}"
        );

        let spaced_oss_variants = exact_identifier_lookup_variants("OSS- 3");
        assert!(
            spaced_oss_variants.iter().any(|variant| variant == "OSS3")
                && spaced_oss_variants.iter().any(|variant| variant == "OSS-3")
                && spaced_oss_variants
                    .iter()
                    .any(|variant| variant == "OSS-  3"),
            "expected spaced OSS variants to include compact and de-spaced forms: {spaced_oss_variants:?}"
        );
    }

    #[test]
    fn compact_identifier_detection_allows_schema_ids_with_internal_spacing() {
        assert!(looks_like_compact_identifier("OSS- 3"));
        assert!(looks_like_compact_identifier("ONS- 2"));
        assert!(looks_like_compact_identifier("PLANT-  4"));
        assert!(!looks_like_compact_identifier("Wind Farm 1"));
        assert!(!looks_like_compact_identifier("Turbine 3"));
    }

    #[test]
    fn lookup_selection_fields_stay_minimal_for_backend_confirmation_queries() {
        let schema = registry();
        let catalog = build_entity_catalog(&schema);
        let family = catalog
            .iter()
            .find(|family| family.type_name == "OffshoreSubstation")
            .expect("expected OffshoreSubstation family");
        let fields =
            lookup_selection_fields(&schema, family, "queryOffshoreSubstation", "shortName");
        assert_eq!(
            fields,
            vec!["shortName".to_string()],
            "confirmation grounding queries should not fail because extra role fields are stale on the live backend"
        );
    }

    #[test]
    fn field_context_collapses_tag_category_comparison_ambiguity() {
        let mut resolutions = vec![
            EntityResolution {
                mention: "Weather".to_string(),
                status: ResolutionStatus::Grounded,
                grounded_matches: vec![GroundedEntityMatch {
                    mention: "Weather".to_string(),
                    family_type: "Tag".to_string(),
                    root_field: "queryTag".to_string(),
                    matched_field: "categoryDescription".to_string(),
                    matched_value: "Weather".to_string(),
                    stable_key_field: Some("categoryDescription".to_string()),
                    stable_key_value: Some("Weather".to_string()),
                    canonical_value: "categoryDescription:Weather".to_string(),
                    display_label: Some("Weather".to_string()),
                }],
                schema_candidates: Vec::new(),
                notes: Vec::new(),
            },
            EntityResolution {
                mention: "Electrical".to_string(),
                status: ResolutionStatus::Ambiguous,
                grounded_matches: vec![
                    GroundedEntityMatch {
                        mention: "Electrical".to_string(),
                        family_type: "Tag".to_string(),
                        root_field: "queryTag".to_string(),
                        matched_field: "categoryDescription".to_string(),
                        matched_value: "Electrical".to_string(),
                        stable_key_field: Some("categoryDescription".to_string()),
                        stable_key_value: Some("Electrical".to_string()),
                        canonical_value: "categoryDescription:Electrical".to_string(),
                        display_label: Some("Electrical".to_string()),
                    },
                    GroundedEntityMatch {
                        mention: "Electrical".to_string(),
                        family_type: "Tag".to_string(),
                        root_field: "queryTag".to_string(),
                        matched_field: "system".to_string(),
                        matched_value: "Electrical".to_string(),
                        stable_key_field: Some("system".to_string()),
                        stable_key_value: Some("Electrical".to_string()),
                        canonical_value: "system:Electrical".to_string(),
                        display_label: Some("Electrical".to_string()),
                    },
                ],
                schema_candidates: Vec::new(),
                notes: Vec::new(),
            },
        ];

        let schema = registry_with_default_sls();
        collapse_field_context_ambiguity(
            &schema,
            r#"Compare tag counts between "Weather" and "Electrical" for plantId "PLANT-  5"."#,
            &mut resolutions,
        );

        let electrical = resolutions
            .iter()
            .find(|resolution| resolution.mention == "Electrical")
            .expect("expected Electrical resolution");
        assert_eq!(electrical.status, ResolutionStatus::Grounded);
        assert_eq!(electrical.grounded_matches.len(), 1);
        assert_eq!(
            electrical.grounded_matches[0].matched_field,
            "categoryDescription"
        );
    }

    #[test]
    fn ambiguity_prefers_stable_entity_over_time_series_observation() {
        let schema = registry();
        let mut resolutions = vec![EntityResolution {
            mention: "the wagon".to_string(),
            status: ResolutionStatus::Ambiguous,
            grounded_matches: vec![
                GroundedEntityMatch {
                    mention: "the wagon".to_string(),
                    family_type: "AisVesselpos".to_string(),
                    root_field: "queryHistoricalAisVesselpos".to_string(),
                    matched_field: "name".to_string(),
                    matched_value: "the wagon".to_string(),
                    stable_key_field: Some("messageTimestamp".to_string()),
                    stable_key_value: Some("2026-02-10T00:30:00Z".to_string()),
                    canonical_value: "the wagon".to_string(),
                    display_label: Some("2026-02-10T00:30:00Z".to_string()),
                },
                GroundedEntityMatch {
                    mention: "the wagon".to_string(),
                    family_type: "Vessel".to_string(),
                    root_field: "queryVessel".to_string(),
                    matched_field: "name".to_string(),
                    matched_value: "the wagon".to_string(),
                    stable_key_field: Some("mmsi".to_string()),
                    stable_key_value: Some("123456789".to_string()),
                    canonical_value: "the wagon".to_string(),
                    display_label: Some("the wagon".to_string()),
                },
            ],
            schema_candidates: Vec::new(),
            notes: Vec::new(),
        }];

        collapse_field_context_ambiguity(
            &schema,
            r#"What's the closest turbine to "the wagon"?"#,
            &mut resolutions,
        );

        assert_eq!(resolutions[0].status, ResolutionStatus::Grounded);
        assert_eq!(resolutions[0].grounded_matches.len(), 1);
        assert_eq!(resolutions[0].grounded_matches[0].root_field, "queryVessel");
        assert_eq!(
            resolutions[0].grounded_matches[0]
                .stable_key_field
                .as_deref(),
            Some("mmsi")
        );
    }

    #[test]
    fn schema_entity_resolution_exposes_status_instead_of_confidence() {
        let schema = registry();
        let resolutions = resolve_entity_resolutions(&schema, "Show details for \"Alpha\".", 4);
        let resolution = resolutions
            .iter()
            .find(|resolution| resolution.mention == "Alpha")
            .expect("expected Alpha resolution");
        assert!(
            matches!(
                resolution.status,
                ResolutionStatus::SchemaCandidate
                    | ResolutionStatus::Ambiguous
                    | ResolutionStatus::Unresolved
            ),
            "unexpected status {:?}",
            resolution.status
        );
        assert!(
            resolution.grounded_matches.is_empty(),
            "expected no grounded matches in schema-only resolver"
        );
    }

    #[test]
    fn entity_resolution_block_reports_schema_only_status() {
        let schema = registry();
        let block = render_entity_resolution_block(&schema, "Show details for \"Alpha\".", 4);
        assert!(
            block.contains("status=`"),
            "expected status in block: {block}"
        );
        assert!(
            block.contains(SCHEMA_CANDIDATE_NOTE),
            "expected schema-only note in block: {block}"
        );
    }

    #[test]
    fn grounded_match_uses_matched_value_and_legacy_canonical_value() {
        let schema = registry();
        let family = build_entity_catalog(&schema)
            .into_iter()
            .find(|family| family.type_name == "OffshoreWindTurbine")
            .expect("expected turbine family");
        let row = json!({
            "name": "Turbine 115",
            "locationId": "TURB-LOC-115",
            "connectedToOffshoreSubstationUid": "OSS-UID-  1",
            "shortName": "T115"
        });

        let grounded = grounded_match_from_row(
            &schema,
            &row,
            &family,
            "queryOffshoreWindTurbine",
            "name",
            "turbine 115",
        )
        .expect("expected grounded match");

        assert_eq!(grounded.matched_value, "Turbine 115");
        assert_eq!(grounded.canonical_value, grounded.matched_value);
    }

    #[test]
    fn grounded_match_uses_role_backed_stable_key_not_relation_uid() {
        let schema = registry();
        let family = build_entity_catalog(&schema)
            .into_iter()
            .find(|family| family.type_name == "OffshoreWindTurbine")
            .expect("expected turbine family");
        let row = json!({
            "name": "Turbine 115",
            "locationId": "TURB-LOC-115",
            "connectedToOffshoreSubstationUid": "OSS-UID-  1",
            "partOfOffshoreWindFarmUid": "FARM-UID-  1",
            "shortName": "T115"
        });

        let grounded = grounded_match_from_row(
            &schema,
            &row,
            &family,
            "queryOffshoreWindTurbine",
            "name",
            "turbine 115",
        )
        .expect("expected grounded match");

        assert!(
            matches!(
                grounded.stable_key_field.as_deref(),
                Some("locationId") | Some("name") | Some("shortName") | Some("sapLocationId")
            ),
            "unexpected stable key field {:?}",
            grounded.stable_key_field
        );
        assert!(
            grounded.stable_key_value.is_some(),
            "expected a role-backed stable key value"
        );
        assert_ne!(grounded.stable_key_value.as_deref(), Some("OSS-UID-  1"));
        assert_ne!(grounded.stable_key_value.as_deref(), Some("FARM-UID-  1"));
    }

    #[test]
    fn grounded_wind_farm_match_prefers_short_name_as_stable_key() {
        let schema = registry();
        let family = build_entity_catalog(&schema)
            .into_iter()
            .find(|family| family.type_name == "OffshoreWindFarm")
            .expect("expected wind farm family");
        let row = json!({
            "name": "Wind Farm 1",
            "plantId": "PLANT-  1",
            "shortName": "WF1"
        });

        let grounded = grounded_match_from_row(
            &schema,
            &row,
            &family,
            "queryOffshoreWindFarm",
            "name",
            "Wind Farm 1",
        )
        .expect("expected grounded match");

        assert!(
            matches!(
                grounded.stable_key_field.as_deref(),
                Some("shortName") | Some("plantId")
            ),
            "expected wind-farm stable key to prefer a stronger identifier than name: {:?}",
            grounded.stable_key_field
        );
        assert!(
            matches!(
                grounded.stable_key_value.as_deref(),
                Some("WF1") | Some("PLANT-  1")
            ),
            "expected wind-farm stable key value to come from shortName or plantId: {:?}",
            grounded.stable_key_value
        );
    }

    #[test]
    fn clear_schema_only_grounding_note_removes_stale_note() {
        let mut notes = vec![
            SCHEMA_CANDIDATE_NOTE.to_string(),
            SCHEMA_ONLY_GROUNDING_NOTE.to_string(),
            "Grounded via exact label lookup on schema-supported roots, prioritized by the request."
                .to_string(),
        ];

        clear_schema_only_grounding_notes(&mut notes);

        assert!(notes.iter().any(|note| note == SCHEMA_CANDIDATE_NOTE));
        assert!(!notes.iter().any(|note| note == SCHEMA_ONLY_GROUNDING_NOTE));
    }

    #[test]
    fn label_grounding_roots_include_schema_supported_roots_beyond_likely_subset() {
        let schema = registry();
        let catalog = build_entity_catalog(&schema);
        let budget = grounding_budget_for_request(
            &schema,
            "What's the closest turbine to \"the wagon\"?",
            None,
            6,
        );
        let roots = label_grounding_roots(
            &schema,
            &catalog,
            "What's the closest turbine to \"the wagon\"?",
            budget,
            None,
        );

        assert!(
            roots.iter().any(|root| root == "queryCable"),
            "expected a non-request schema-supported root such as queryCable in label grounding roots, got {:?}",
            roots
        );
    }

    #[test]
    fn grounding_budget_expands_for_analytical_label_queries() {
        let schema = registry();
        let budget = grounding_budget_for_request(
            &schema,
            r#"Compare average accumulatedWindDowntime between "Wind Farm 3" and "Wind Farm 4""#,
            Some("Wind Farm 3"),
            6,
        );

        assert!(
            budget.root_guided_label_root_limit > DEFAULT_ROOT_GUIDED_LABEL_ROOTS,
            "expected broader root budget, got {budget:?}"
        );
        assert!(
            budget.grounded_match_limit > DEFAULT_GROUNDED_MATCHES_PER_MENTION,
            "expected broader match budget, got {budget:?}"
        );
        assert!(
            budget.schema_candidate_limit >= 7,
            "expected broader schema candidate preview, got {budget:?}"
        );
    }

    #[test]
    fn grounding_budget_tightens_for_compact_identifier_mentions() {
        let schema = registry();
        let budget =
            grounding_budget_for_request(&schema, "Show details for OSS-003", Some("OSS-003"), 6);

        assert!(
            budget.prioritized_root_limit <= 6,
            "expected tighter prioritized root budget, got {budget:?}"
        );
        assert!(
            budget.grounded_match_limit <= 3,
            "expected tighter grounded-match budget, got {budget:?}"
        );
    }

    #[test]
    fn grounding_budget_expands_from_schema_ambiguity() {
        let schema = registry();
        let resolution = super::EntityResolution {
            mention: "Wind Farm 3".to_string(),
            status: ResolutionStatus::Ambiguous,
            grounded_matches: vec![],
            schema_candidates: vec![
                SchemaEntityCandidate {
                    family_type: "OffshoreWindFarm".to_string(),
                    lookup_roots: vec!["queryOffshoreWindFarm".to_string()],
                    key_fields: vec!["shortName".to_string()],
                    label_fields: vec!["name".to_string()],
                    filter_fields: vec![],
                },
                SchemaEntityCandidate {
                    family_type: "OffshoreSubstation".to_string(),
                    lookup_roots: vec!["queryOffshoreSubstation".to_string()],
                    key_fields: vec!["shortName".to_string()],
                    label_fields: vec!["name".to_string()],
                    filter_fields: vec![],
                },
                SchemaEntityCandidate {
                    family_type: "OnshoreSubstation".to_string(),
                    lookup_roots: vec!["queryOnshoreSubstation".to_string()],
                    key_fields: vec!["shortName".to_string()],
                    label_fields: vec!["name".to_string()],
                    filter_fields: vec![],
                },
            ],
            notes: vec![],
        };

        let base = grounding_budget_for_request(
            &schema,
            "Compare average accumulatedWindDowntime between Wind Farm 3 and Wind Farm 4",
            Some("Wind Farm 3"),
            6,
        );
        let expanded = grounding_budget_for_resolution(
            &schema,
            "Compare average accumulatedWindDowntime between Wind Farm 3 and Wind Farm 4",
            &resolution,
            6,
        );

        assert!(
            expanded.root_guided_label_root_limit > base.root_guided_label_root_limit,
            "expected schema ambiguity to widen root budget: base={base:?} expanded={expanded:?}"
        );
        assert!(
            expanded.grounded_match_limit >= base.grounded_match_limit,
            "expected schema ambiguity to preserve or widen match budget: base={base:?} expanded={expanded:?}"
        );
    }

    #[test]
    fn label_grounding_roots_prioritize_schema_candidate_roots() {
        let schema = registry();
        let catalog = build_entity_catalog(&schema);
        let resolution = super::EntityResolution {
            mention: "mystery thing".to_string(),
            status: ResolutionStatus::Ambiguous,
            grounded_matches: vec![],
            schema_candidates: vec![SchemaEntityCandidate {
                family_type: "Cable".to_string(),
                lookup_roots: vec!["queryCable".to_string()],
                key_fields: vec!["shortName".to_string()],
                label_fields: vec!["name".to_string()],
                filter_fields: vec![],
            }],
            notes: vec![],
        };
        let roots = label_grounding_roots(
            &schema,
            &catalog,
            "mystery thing",
            super::GroundingBudget {
                schema_candidate_limit: 6,
                prioritized_root_limit: 6,
                root_guided_label_root_limit: 8,
                grounded_match_limit: 4,
            },
            Some(&resolution),
        );

        let cable_index = roots
            .iter()
            .position(|root| root == "queryCable")
            .expect("expected queryCable in candidate-guided roots");
        let anchor_index = roots
            .iter()
            .position(|root| root == "queryAnchorArea")
            .expect("expected generic fallback root like queryAnchorArea");

        assert!(
            cable_index < anchor_index,
            "expected schema-candidate root queryCable to be injected before generic fallback roots, got {roots:?}"
        );
    }

    #[test]
    fn schema_resolution_budget_uses_retrieval_signal_for_generic_queries() {
        let schema = registry();
        let catalog = build_entity_catalog(&schema);
        let resolutions = schema_resolutions_from_mentions(
            &schema,
            &catalog,
            "Show details",
            vec!["Alpha".to_string()],
            4,
        );

        assert_eq!(resolutions.len(), 1);
        assert!(
            resolutions[0].schema_candidates.len() >= 4,
            "expected low-confidence retrieval to preserve a broader schema candidate preview: {:?}",
            resolutions[0].schema_candidates
        );
    }

    #[test]
    fn entity_resolution_block_renders_matched_value_and_stable_key() {
        let block = render_entity_resolution_block_from_resolutions(&[super::EntityResolution {
            mention: "turbine 115".to_string(),
            status: ResolutionStatus::Grounded,
            grounded_matches: vec![GroundedEntityMatch {
                mention: "turbine 115".to_string(),
                family_type: "OffshoreWindTurbine".to_string(),
                root_field: "queryOffshoreWindTurbine".to_string(),
                matched_field: "name".to_string(),
                matched_value: "Turbine 115".to_string(),
                stable_key_field: Some("shortName".to_string()),
                stable_key_value: Some("T115".to_string()),
                canonical_value: "Turbine 115".to_string(),
                display_label: Some("Turbine 115".to_string()),
            }],
            schema_candidates: vec![],
            notes: vec![],
        }]);

        assert!(
            block.contains("matched_value=`Turbine 115`"),
            "expected matched_value in block: {block}"
        );
        assert!(
            block.contains("stable_key=`shortName:T115`"),
            "expected stable_key in block: {block}"
        );
        assert!(
            block.contains("label=`Turbine 115`"),
            "expected label in block: {block}"
        );
        assert!(
            !block.contains(" value=`"),
            "expected old value label to be absent: {block}"
        );
    }

    #[test]
    fn insert_grounded_match_collapses_duplicate_lookup_roots_for_same_entity() {
        let mut grounded_matches = vec![GroundedEntityMatch {
            mention: "WF3".to_string(),
            family_type: "OffshoreWindFarm".to_string(),
            root_field: "batchGetOffshoreWindFarm".to_string(),
            matched_field: "shortName".to_string(),
            matched_value: "WF3".to_string(),
            stable_key_field: Some("shortName".to_string()),
            stable_key_value: Some("WF3".to_string()),
            canonical_value: "WF3".to_string(),
            display_label: Some("Wind Farm 3".to_string()),
        }];

        insert_grounded_match(
            &mut grounded_matches,
            GroundedEntityMatch {
                mention: "WF3".to_string(),
                family_type: "OffshoreWindFarm".to_string(),
                root_field: "getOffshoreWindFarm".to_string(),
                matched_field: "shortName".to_string(),
                matched_value: "WF3".to_string(),
                stable_key_field: Some("shortName".to_string()),
                stable_key_value: Some("WF3".to_string()),
                canonical_value: "WF3".to_string(),
                display_label: Some("Wind Farm 3".to_string()),
            },
        );

        assert_eq!(grounded_matches.len(), 1);
        assert_eq!(grounded_matches[0].root_field, "getOffshoreWindFarm");
    }
}
