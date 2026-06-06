#![allow(dead_code)]

use crate::schema_registry::{RetrievedRootSlice, SchemaRegistry, SchemaRetrievalSlice};
use crate::sls::{IntentVocabulary, Sls};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

const ROOT_RECALL_SCORE_CEILING: i32 = 80;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CapabilityGraph {
    roots: BTreeMap<String, CapabilityRoot>,
    metrics: BTreeMap<String, CapabilityMetric>,
    preferred_join_paths: Vec<CapabilityJoinPath>,
    intent_vocabulary: IntentVocabulary,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CapabilityRoot {
    pub(crate) root_field: String,
    pub(crate) return_type: String,
    pub(crate) scalar_fields: BTreeSet<String>,
    pub(crate) relation_fields: BTreeMap<String, String>,
    pub(crate) filter_fields: BTreeSet<String>,
    pub(crate) identifier_filter_fields: BTreeSet<String>,
    pub(crate) time_filter_fields: BTreeSet<String>,
    pub(crate) numeric_fields: BTreeSet<String>,
    pub(crate) time_fields: BTreeSet<String>,
    pub(crate) label_fields: BTreeSet<String>,
    pub(crate) concept_aliases: BTreeSet<String>,
    pub(crate) explicit_concept_aliases: BTreeSet<String>,
    pub(crate) generated_concept_aliases: BTreeSet<String>,
    pub(crate) semantic_field_defaults: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CapabilityMetric {
    pub(crate) name: String,
    pub(crate) aliases: BTreeSet<String>,
    pub(crate) source_type: String,
    pub(crate) unit: Option<String>,
    pub(crate) time_field: Option<String>,
    pub(crate) duration_field: Option<String>,
    pub(crate) aggregation: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CapabilityJoinPath {
    from_root: String,
    to_root: String,
    strategy: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CapabilityPath {
    pub(crate) from_root: String,
    pub(crate) target_type: String,
    pub(crate) path: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CapabilityGap {
    pub(crate) root_field: String,
    pub(crate) reason: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CapabilityTaskShape {
    rank_like: bool,
    trend_like: bool,
    compare_like: bool,
    aggregate_like: bool,
    membership_like: bool,
    numeric_metric_requested: bool,
    temporal_scope_requested: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct MembershipJoinEvidence {
    mentioned_roots: BTreeSet<String>,
    preferred_join_evidence_by_root: BTreeMap<String, Vec<String>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct CapabilityRootEvaluation {
    bonus: i32,
    evidence: Vec<String>,
}

impl CapabilityGraph {
    pub(crate) fn from_registry(schema_registry: &SchemaRegistry, sls: Option<&Sls>) -> Self {
        let sls_ref = sls;
        let roots = schema_registry
            .query_root_fields()
            .into_iter()
            .filter_map(|root_field| {
                let return_type = schema_registry.query_return_type(&root_field)?.to_string();
                let object_fields = schema_registry.object_field_names(&return_type)?;
                let roles = schema_registry.field_roles_for_root(&root_field);

                let mut scalar_fields = BTreeSet::new();
                let mut relation_fields = BTreeMap::new();
                for field in object_fields {
                    let Some(field_type) = schema_registry.object_field_type(&return_type, field)
                    else {
                        continue;
                    };
                    if schema_registry.object_field_names(field_type).is_some() {
                        relation_fields.insert(field.clone(), field_type.to_string());
                    } else {
                        scalar_fields.insert(field.clone());
                    }
                }

                let filter_fields = schema_registry
                    .root_filter_fields(&root_field)
                    .into_iter()
                    .collect::<BTreeSet<_>>();
                let identifier_filter_fields = schema_registry
                    .root_identifier_filter_fields(&root_field)
                    .into_iter()
                    .collect::<BTreeSet<_>>();
                let time_filter_fields = schema_registry
                    .root_time_filter_fields(&root_field)
                    .into_iter()
                    .collect::<BTreeSet<_>>();
                let numeric_fields = roles
                    .numeric_fields
                    .into_iter()
                    .filter(|field| scalar_fields.contains(field))
                    .collect::<BTreeSet<_>>();
                let time_fields = roles
                    .time_fields
                    .into_iter()
                    .filter(|field| scalar_fields.contains(field))
                    .collect::<BTreeSet<_>>();
                let label_fields = roles
                    .label_fields
                    .into_iter()
                    .filter(|field| scalar_fields.contains(field))
                    .collect::<BTreeSet<_>>();
                let concept_aliases = schema_registry
                    .concept_aliases_for_type(&return_type)
                    .into_iter()
                    .collect::<BTreeSet<_>>();
                let explicit_concept_aliases = schema_registry
                    .explicit_concept_aliases_for_type(&return_type)
                    .into_iter()
                    .collect::<BTreeSet<_>>();
                let generated_concept_aliases = concept_aliases
                    .difference(&explicit_concept_aliases)
                    .cloned()
                    .collect::<BTreeSet<_>>();
                let semantic_field_defaults =
                    semantic_field_defaults_for_root(sls_ref, &root_field, &return_type);

                Some((
                    root_field.clone(),
                    CapabilityRoot {
                        root_field,
                        return_type,
                        scalar_fields,
                        relation_fields,
                        filter_fields,
                        identifier_filter_fields,
                        time_filter_fields,
                        numeric_fields,
                        time_fields,
                        label_fields,
                        concept_aliases,
                        explicit_concept_aliases,
                        generated_concept_aliases,
                        semantic_field_defaults,
                    },
                ))
            })
            .collect::<BTreeMap<_, _>>();

        let metrics = sls
            .and_then(|sls| sls.metrics.as_ref())
            .map(|metrics| {
                metrics
                    .iter()
                    .map(|(name, metric)| {
                        (
                            name.clone(),
                            CapabilityMetric {
                                name: name.clone(),
                                aliases: metric.aliases.iter().cloned().collect(),
                                source_type: metric.source.type_name.clone(),
                                unit: metric.unit.clone(),
                                time_field: metric.source.time_field.clone(),
                                duration_field: metric.source.duration_field.clone(),
                                aggregation: metric.aggregation.clone(),
                            },
                        )
                    })
                    .collect::<BTreeMap<_, _>>()
            })
            .unwrap_or_default();

        let intent_vocabulary = sls
            .map(|sls| sls.intent_vocabulary.clone())
            .unwrap_or_default();
        let preferred_join_paths = sls
            .map(|sls| {
                sls.preferred_join_paths
                    .iter()
                    .map(|join| CapabilityJoinPath {
                        from_root: join.from_root.clone(),
                        to_root: join.to_root.clone(),
                        strategy: join.strategy.clone(),
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        Self {
            roots,
            metrics,
            preferred_join_paths,
            intent_vocabulary,
        }
    }

    pub(crate) fn root(&self, root_field: &str) -> Option<&CapabilityRoot> {
        self.roots.get(root_field)
    }

    pub(crate) fn roots(&self) -> impl Iterator<Item = &CapabilityRoot> {
        self.roots.values()
    }

    pub(crate) fn metric(&self, metric_name: &str) -> Option<&CapabilityMetric> {
        self.metrics.get(metric_name)
    }

    pub(crate) fn can_filter_field(&self, root_field: &str, field: &str) -> bool {
        self.root(root_field)
            .is_some_and(|root| root.filter_fields.contains(field))
    }

    pub(crate) fn can_select_path(
        &self,
        schema_registry: &SchemaRegistry,
        root_field: &str,
        path: &str,
    ) -> bool {
        self.selection_leaf_type(schema_registry, root_field, path)
            .is_some()
    }

    pub(crate) fn selection_leaf_type(
        &self,
        schema_registry: &SchemaRegistry,
        root_field: &str,
        path: &str,
    ) -> Option<String> {
        let root = self.root(root_field)?;
        let parts = path
            .split('.')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>();
        if parts.is_empty() {
            return None;
        }

        let mut current_type = root.return_type.as_str();
        for (idx, part) in parts.iter().enumerate() {
            let field_names = schema_registry.object_field_names(current_type)?;
            let field_name = field_names
                .iter()
                .find(|field| field.as_str() == *part)
                .or_else(|| {
                    field_names
                        .iter()
                        .find(|field| field.eq_ignore_ascii_case(part))
                })?;
            let field_type = schema_registry.object_field_type(current_type, field_name)?;
            let is_object = schema_registry.object_field_names(field_type).is_some();
            let is_last = idx == parts.len() - 1;
            if is_last {
                return (!is_object).then(|| field_type.to_string());
            }
            if !is_object {
                return None;
            }
            current_type = field_type;
        }
        None
    }

    pub(crate) fn relation_paths_to_type(
        &self,
        schema_registry: &SchemaRegistry,
        from_root: &str,
        target_type: &str,
        max_depth: usize,
    ) -> Vec<CapabilityPath> {
        let Some(root) = self.root(from_root) else {
            return Vec::new();
        };
        if max_depth == 0 {
            return Vec::new();
        }

        let mut out = Vec::new();
        let mut queue = VecDeque::from([(root.return_type.clone(), Vec::<String>::new())]);
        let mut visited = BTreeSet::new();

        while let Some((current_type, path)) = queue.pop_front() {
            if path.len() >= max_depth {
                continue;
            }
            let visit_key = format!("{}|{}", current_type, path.join("."));
            if !visited.insert(visit_key) {
                continue;
            }
            let Some(fields) = schema_registry.object_field_names(&current_type) else {
                continue;
            };
            for field in fields {
                let Some(field_type) = schema_registry.object_field_type(&current_type, field)
                else {
                    continue;
                };
                if schema_registry.object_field_names(field_type).is_none() {
                    continue;
                }
                let mut next_path = path.clone();
                next_path.push(field.clone());
                if field_type == target_type {
                    out.push(CapabilityPath {
                        from_root: from_root.to_string(),
                        target_type: target_type.to_string(),
                        path: next_path.join("."),
                    });
                }
                queue.push_back((field_type.to_string(), next_path));
            }
        }

        out.sort_by(|a, b| a.path.cmp(&b.path));
        out.dedup_by(|a, b| a.path == b.path && a.target_type == b.target_type);
        out
    }

    pub(crate) fn missing_root_gap(root_field: &str) -> CapabilityGap {
        CapabilityGap {
            root_field: root_field.to_string(),
            reason: "root field is not present in the active schema".to_string(),
        }
    }

    pub(crate) fn refine_retrieval_slice(
        &self,
        mut slice: SchemaRetrievalSlice,
        query: &str,
        root_limit: usize,
    ) -> SchemaRetrievalSlice {
        let tokens = query_tokens(query);
        let phrases = adjacent_phrases(&tokens);
        let mut task = CapabilityTaskShape::from_tokens(&tokens, &self.intent_vocabulary);
        if self.query_mentions_numeric_capability(&tokens, &phrases) {
            task.numeric_metric_requested = true;
        }
        let membership_evidence = self.membership_join_evidence(&tokens, &phrases, task);

        for root in &mut slice.roots {
            let evaluation =
                self.capability_evaluation(root, &tokens, &phrases, task, &membership_evidence);
            let recall_score = root.score.clamp(0, ROOT_RECALL_SCORE_CEILING);
            root.score = recall_score + evaluation.bonus;
            root.capability_evidence = evaluation.evidence;
        }
        slice
            .roots
            .sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.root.cmp(&b.root)));
        let exact_numeric_match_available = task.rank_like
            && slice.roots.iter().any(|root| {
                self.root(&root.root).is_some_and(|capability| {
                    exact_field_match_bonus(
                        tokens.as_slice(),
                        phrases.as_slice(),
                        &capability.numeric_fields,
                        1,
                    ) > 0
                })
            });
        let capable_roots = slice
            .roots
            .iter()
            .filter(|root| {
                if !membership_evidence
                    .preferred_join_evidence_by_root
                    .is_empty()
                    && !membership_evidence
                        .preferred_join_evidence_by_root
                        .contains_key(&root.root)
                {
                    return false;
                }
                if exact_numeric_match_available
                    && self.root(&root.root).is_some_and(|capability| {
                        exact_field_match_bonus(
                            tokens.as_slice(),
                            phrases.as_slice(),
                            &capability.numeric_fields,
                            1,
                        ) == 0
                    })
                {
                    return false;
                }
                self.root_satisfies_task(root, task)
            })
            .cloned()
            .collect::<Vec<_>>();
        if !capable_roots.is_empty() {
            slice.roots = capable_roots;
        }
        slice.roots.truncate(root_limit.max(1));
        slice
    }

    fn capability_evaluation(
        &self,
        root_slice: &RetrievedRootSlice,
        tokens: &[String],
        phrases: &[String],
        task: CapabilityTaskShape,
        membership_evidence: &MembershipJoinEvidence,
    ) -> CapabilityRootEvaluation {
        let Some(root) = self.root(&root_slice.root) else {
            return CapabilityRootEvaluation::default();
        };
        let mut evidence = Vec::new();
        let mut bonus = 0;
        bonus += overlap_bonus(tokens, phrases, &root.scalar_fields, 12);
        bonus += overlap_bonus(tokens, phrases, &root.filter_fields, 10);
        bonus += overlap_bonus(tokens, phrases, root.relation_fields.keys(), 14);
        bonus += overlap_bonus(tokens, phrases, &root.label_fields, 8);
        let explicit_concept_overlap =
            overlap_bonus(tokens, phrases, &root.explicit_concept_aliases, 60);
        let explicit_concept_exact =
            exact_field_match_bonus(tokens, phrases, &root.explicit_concept_aliases, 420);
        let explicit_concept_matches =
            matching_values(tokens, phrases, &root.explicit_concept_aliases);
        if !explicit_concept_matches.is_empty() {
            evidence.push(format!(
                "sls_concept_match:{}",
                explicit_concept_matches.join(",")
            ));
        }
        bonus += explicit_concept_overlap + explicit_concept_exact;
        let generated_concept_overlap =
            overlap_bonus(tokens, phrases, &root.generated_concept_aliases, 12);
        let generated_concept_matches =
            matching_values(tokens, phrases, &root.generated_concept_aliases);
        if !generated_concept_matches.is_empty() {
            evidence.push(format!(
                "generated_concept_recall:{}",
                generated_concept_matches.join(",")
            ));
        }
        bonus += generated_concept_overlap;
        bonus += overlap_bonus(tokens, phrases, &root.numeric_fields, 16);
        bonus += overlap_bonus(tokens, phrases, &root.time_fields, 16);
        bonus += exact_field_match_bonus(tokens, phrases, &root.numeric_fields, 300);
        bonus += exact_field_match_bonus(tokens, phrases, &root.scalar_fields, 220);
        let semantic_bonus =
            semantic_field_default_bonus(tokens, phrases, &root.semantic_field_defaults, 360);
        if semantic_bonus > 0 {
            evidence.push(format!(
                "sls_field_default_match:{}",
                matching_default_aliases(tokens, phrases, &root.semantic_field_defaults).join(",")
            ));
        }
        bonus += semantic_bonus;
        let (metric_bonus, metric_evidence) =
            self.metric_alias_bonus(tokens, phrases, &root.return_type);
        bonus += metric_bonus;
        evidence.extend(metric_evidence);
        if let Some(join_evidence) = membership_evidence
            .preferred_join_evidence_by_root
            .get(&root.root_field)
        {
            bonus += 900;
            evidence.extend(join_evidence.iter().cloned());
        } else if task.membership_like
            && membership_evidence
                .mentioned_roots
                .contains(&root.root_field)
        {
            bonus += 120;
            evidence.push("sls_membership_concept_match".to_string());
        }

        if task.rank_like && !root.numeric_fields.is_empty() {
            bonus += 36;
        }
        if task.compare_like && task.numeric_metric_requested && !root.numeric_fields.is_empty() {
            bonus += 24;
        }
        if task.aggregate_like && !root.numeric_fields.is_empty() {
            bonus += 16;
        }
        if task.temporal_scope_requested
            && (!root.time_fields.is_empty() || !root.time_filter_fields.is_empty())
        {
            bonus += 36;
        }
        if root.identifier_filter_fields.iter().any(|field| {
            tokens.iter().any(|token| {
                normalized(field).contains(token) || token.contains(&normalized(field))
            })
        }) {
            bonus += 16;
        }
        evidence.sort();
        evidence.dedup();
        CapabilityRootEvaluation { bonus, evidence }
    }

    fn metric_alias_bonus(
        &self,
        tokens: &[String],
        phrases: &[String],
        return_type: &str,
    ) -> (i32, Vec<String>) {
        let mut bonus = 0;
        let mut evidence = Vec::new();
        for metric in self
            .metrics
            .values()
            .filter(|metric| metric.source_type.eq_ignore_ascii_case(return_type))
        {
            let mut aliases = metric.aliases.iter().cloned().collect::<Vec<_>>();
            aliases.push(metric.name.clone());
            let metric_bonus = overlap_bonus(tokens, phrases, &aliases, 120)
                + exact_field_match_bonus(tokens, phrases, &aliases, 240);
            if metric_bonus > 0 {
                bonus += metric_bonus;
                evidence.push(format!("sls_metric_match:{}", metric.name));
            }
        }
        (bonus, evidence)
    }

    fn membership_join_evidence(
        &self,
        tokens: &[String],
        phrases: &[String],
        task: CapabilityTaskShape,
    ) -> MembershipJoinEvidence {
        if !task.membership_like {
            return MembershipJoinEvidence::default();
        }

        let mentioned_roots = self
            .roots
            .values()
            .filter(|root| {
                !matching_values(tokens, phrases, &root.explicit_concept_aliases).is_empty()
            })
            .map(|root| root.root_field.clone())
            .collect::<BTreeSet<_>>();
        if mentioned_roots.len() < 2 {
            return MembershipJoinEvidence {
                mentioned_roots,
                preferred_join_evidence_by_root: BTreeMap::new(),
            };
        }

        let mut preferred_join_evidence_by_root: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for join in &self.preferred_join_paths {
            if join.strategy.as_deref() != Some("parent_relation") {
                continue;
            }
            if !mentioned_roots.contains(&join.from_root)
                || !mentioned_roots.contains(&join.to_root)
            {
                continue;
            }
            let relation_fields =
                self.relation_fields_between_roots(&join.from_root, &join.to_root);
            if relation_fields.is_empty() {
                continue;
            }
            let evidence = format!(
                "sls_preferred_join_match:{}->{}:{}:{}",
                join.from_root,
                join.to_root,
                join.strategy.as_deref().unwrap_or("unknown"),
                relation_fields.join("|")
            );
            preferred_join_evidence_by_root
                .entry(join.from_root.clone())
                .or_default()
                .push(evidence.clone());
            preferred_join_evidence_by_root
                .entry(join.to_root.clone())
                .or_default()
                .push(evidence);
        }

        MembershipJoinEvidence {
            mentioned_roots,
            preferred_join_evidence_by_root,
        }
    }

    fn query_mentions_numeric_capability(&self, tokens: &[String], phrases: &[String]) -> bool {
        let metric_match = self.metrics.values().any(|metric| {
            let mut aliases = metric.aliases.iter().cloned().collect::<Vec<_>>();
            aliases.push(metric.name.clone());
            !matching_values(tokens, phrases, &aliases).is_empty()
                && self.roots.values().any(|root| {
                    root.return_type.eq_ignore_ascii_case(&metric.source_type)
                        && !root.numeric_fields.is_empty()
                })
        });
        if metric_match {
            return true;
        }

        self.roots.values().any(|root| {
            !matching_values(tokens, phrases, &root.numeric_fields).is_empty()
                || root
                    .semantic_field_defaults
                    .iter()
                    .any(|(alias_norm, field_name)| {
                        root.numeric_fields.contains(field_name)
                            && (normalized_query_match(tokens, phrases, alias_norm)
                                || normalized_query_match(tokens, phrases, &normalized(field_name)))
                    })
        })
    }

    fn relation_fields_between_roots(&self, from_root: &str, to_root: &str) -> Vec<String> {
        let Some(from) = self.root(from_root) else {
            return Vec::new();
        };
        let Some(to) = self.root(to_root) else {
            return Vec::new();
        };
        let mut relation_fields = from
            .relation_fields
            .iter()
            .filter(|(_, field_type)| field_type.eq_ignore_ascii_case(&to.return_type))
            .map(|(field_name, _)| field_name.clone())
            .collect::<Vec<_>>();
        relation_fields.sort();
        relation_fields
    }

    fn root_satisfies_task(
        &self,
        root_slice: &RetrievedRootSlice,
        task: CapabilityTaskShape,
    ) -> bool {
        let Some(root) = self.root(&root_slice.root) else {
            return false;
        };
        if task.trend_like
            && (root.numeric_fields.is_empty()
                || (root.time_fields.is_empty() && root.time_filter_fields.is_empty()))
        {
            return false;
        }
        if task.rank_like && root.numeric_fields.is_empty() {
            return false;
        }
        if task.compare_like && task.numeric_metric_requested && root.numeric_fields.is_empty() {
            return false;
        }
        if task.temporal_scope_requested
            && root.time_fields.is_empty()
            && root.time_filter_fields.is_empty()
            && !task.compare_like
            && !task.rank_like
        {
            return false;
        }
        true
    }
}

impl CapabilityTaskShape {
    fn from_tokens(tokens: &[String], vocabulary: &IntentVocabulary) -> Self {
        let rank_like = tokens_match_terms(tokens, &vocabulary.rank_desc)
            || tokens_match_terms(tokens, &vocabulary.rank_asc);
        let trend_like = tokens_match_terms(tokens, &vocabulary.trend);
        let compare_like = tokens_match_terms(tokens, &vocabulary.compare);
        let aggregate_like = tokens_match_terms(tokens, &vocabulary.aggregate_count)
            || tokens_match_terms(tokens, &vocabulary.aggregate_avg)
            || tokens_match_terms(tokens, &vocabulary.aggregate_sum)
            || tokens_match_terms(tokens, &vocabulary.group_by)
            || rank_like;
        let membership_like = tokens_match_terms(tokens, &vocabulary.membership);
        let numeric_metric_requested = rank_like
            || tokens_match_terms(tokens, &vocabulary.aggregate_avg)
            || tokens_match_terms(tokens, &vocabulary.aggregate_sum);
        let temporal_scope_requested =
            trend_like || tokens_match_terms(tokens, &vocabulary.temporal);

        Self {
            rank_like,
            trend_like,
            compare_like,
            aggregate_like,
            membership_like,
            numeric_metric_requested,
            temporal_scope_requested,
        }
    }
}

fn query_tokens(query: &str) -> Vec<String> {
    let mut out = Vec::new();
    for token in query
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
    {
        let token = token.to_ascii_lowercase();
        if token.len() < 2 && !token.chars().any(|ch| ch.is_ascii_digit()) {
            continue;
        }
        if !out.iter().any(|existing| existing == &token) {
            out.push(token);
        }
    }
    out
}

fn adjacent_phrases(tokens: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for window in tokens.windows(2) {
        let phrase = format!("{}{}", window[0], window[1]);
        if !out.iter().any(|existing| existing == &phrase) {
            out.push(phrase);
        }
    }
    out
}

fn tokens_match_terms(tokens: &[String], terms: &[String]) -> bool {
    let token_set = tokens.iter().map(String::as_str).collect::<Vec<_>>();
    terms.iter().any(|term| {
        let normalized_term = normalized(term);
        if normalized_term.is_empty() {
            return false;
        }
        token_set.iter().any(|token| *token == normalized_term)
            || adjacent_phrases(tokens)
                .iter()
                .any(|phrase| phrase == &normalized_term)
    })
}

fn normalized(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .map(|ch| ch.to_ascii_lowercase())
        .collect()
}

fn semantic_field_defaults_for_root(
    sls: Option<&Sls>,
    root_field: &str,
    return_type: &str,
) -> BTreeMap<String, String> {
    let Some(sls) = sls else {
        return BTreeMap::new();
    };
    let mut out = BTreeMap::new();
    if let Some(defaults) = sls.canonical_field_defaults.by_type.get(return_type) {
        for (alias, field) in defaults {
            out.insert(normalized(alias), field.clone());
        }
    }
    if let Some(defaults) = sls.canonical_field_defaults.by_root.get(root_field) {
        for (alias, field) in defaults {
            out.insert(normalized(alias), field.clone());
        }
    }
    out
}

fn normalized_query_match(tokens: &[String], phrases: &[String], value_norm: &str) -> bool {
    !value_norm.is_empty()
        && (tokens.iter().any(|token| token == value_norm)
            || phrases.iter().any(|phrase| phrase == value_norm))
}

fn matching_values<'a>(
    tokens: &[String],
    phrases: &[String],
    values: impl IntoIterator<Item = &'a String>,
) -> Vec<String> {
    let mut out = Vec::new();
    for value in values {
        let value_norm = normalized(value);
        if value_norm.is_empty() {
            continue;
        }
        if normalized_query_match(tokens, phrases, &value_norm) {
            out.push(value.clone());
        }
    }
    out.sort();
    out.dedup();
    out
}

fn matching_default_aliases(
    tokens: &[String],
    phrases: &[String],
    defaults: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut out = Vec::new();
    for (alias_norm, field_name) in defaults {
        let field_norm = normalized(field_name);
        if tokens
            .iter()
            .any(|token| token == alias_norm || token == &field_norm)
            || phrases
                .iter()
                .any(|phrase| phrase == alias_norm || phrase == &field_norm)
        {
            out.push(format!("{alias_norm}->{field_name}"));
        }
    }
    out.sort();
    out.dedup();
    out
}

fn overlap_bonus<'a>(
    tokens: &[String],
    phrases: &[String],
    values: impl IntoIterator<Item = &'a String>,
    weight: i32,
) -> i32 {
    let mut score = 0;
    for value in values {
        let value_norm = normalized(value);
        if value_norm.is_empty() {
            continue;
        }
        if tokens.iter().any(|token| {
            value_norm == *token || value_norm.contains(token) || token.contains(&value_norm)
        }) {
            score += weight;
        }
        if phrases.iter().any(|phrase| {
            value_norm == *phrase || value_norm.contains(phrase) || phrase.contains(&value_norm)
        }) {
            score += weight * 2;
        }
    }
    score
}

fn exact_field_match_bonus<'a>(
    tokens: &[String],
    phrases: &[String],
    values: impl IntoIterator<Item = &'a String>,
    weight: i32,
) -> i32 {
    let mut score = 0;
    for value in values {
        let value_norm = normalized(value);
        if value_norm.is_empty() {
            continue;
        }
        if tokens.iter().any(|token| token == &value_norm)
            || phrases.iter().any(|phrase| phrase == &value_norm)
        {
            score += weight;
        }
    }
    score
}

fn semantic_field_default_bonus(
    tokens: &[String],
    phrases: &[String],
    defaults: &BTreeMap<String, String>,
    weight: i32,
) -> i32 {
    let mut score = 0;
    for (alias_norm, field_name) in defaults {
        if tokens.iter().any(|token| token == alias_norm)
            || phrases.iter().any(|phrase| phrase == alias_norm)
        {
            score += weight;
        }
        let field_norm = normalized(field_name);
        if tokens.iter().any(|token| token == &field_norm)
            || phrases.iter().any(|phrase| phrase == &field_norm)
        {
            score += weight;
        }
    }
    score
}

#[cfg(test)]
mod tests {
    use super::CapabilityGraph;
    use crate::schema_registry::{
        QueryRootRetrievalProfile, RetrievalConfidence, RetrievedRootSlice, SchemaRegistry,
        SchemaRetrievalSlice,
    };

    fn graph() -> (SchemaRegistry, CapabilityGraph) {
        let bootstrap = SchemaRegistry::new(include_str!("../schemas/consumer_schema.graphql"));
        let sls = crate::sls::load_sls_merged(&bootstrap, "sls.yaml").expect("expected SLS");
        let schema = SchemaRegistry::with_sls(
            include_str!("../schemas/consumer_schema.graphql"),
            Some(&sls),
        );
        let graph = CapabilityGraph::from_registry(&schema, Some(&sls));
        (schema, graph)
    }

    fn root_slice(root: &str, return_type: &str, score: i32) -> RetrievedRootSlice {
        RetrievedRootSlice {
            root: root.to_string(),
            score,
            capability_evidence: Vec::new(),
            return_type: return_type.to_string(),
            concept_aliases: Vec::new(),
            key_fields: Vec::new(),
            intent_fields: Vec::new(),
            default_scalar_fields: Vec::new(),
            numeric_fields: Vec::new(),
            time_fields: Vec::new(),
            relation_fields: Vec::new(),
            filter_fields: Vec::new(),
            identifier_filter_fields: Vec::new(),
            time_filter_fields: Vec::new(),
        }
    }

    #[test]
    fn capability_graph_lists_root_scalars() {
        let (_schema, graph) = graph();
        let farm = graph
            .root("queryOffshoreWindFarm")
            .expect("expected wind farm root");

        assert!(farm.scalar_fields.contains("name"));
        assert!(farm.scalar_fields.contains("shortName"));
        assert!(farm.scalar_fields.contains("ratedCapacity"));
    }

    #[test]
    fn capability_graph_validates_nested_relation_leaf() {
        let (schema, graph) = graph();

        assert!(graph.can_select_path(
            &schema,
            "queryOffshoreWindFarm",
            "hasOffshoreWindTurbine.name"
        ));
    }

    #[test]
    fn capability_graph_rejects_object_without_leaf() {
        let (schema, graph) = graph();

        assert!(!graph.can_select_path(&schema, "queryOffshoreWindTurbine", "location"));
    }

    #[test]
    fn capability_graph_lists_filter_fields() {
        let (_schema, graph) = graph();
        let turbine = graph
            .root("queryOffshoreWindTurbine")
            .expect("expected turbine root");

        assert!(turbine.filter_fields.contains("name"));
        assert!(turbine.filter_fields.contains("shortName"));
        assert!(graph.can_filter_field("queryOffshoreWindTurbine", "locationId"));
    }

    #[test]
    fn capability_graph_finds_parent_child_path() {
        let (schema, graph) = graph();
        let paths = graph.relation_paths_to_type(
            &schema,
            "queryOffshoreWindFarm",
            "OffshoreWindTurbine",
            2,
        );

        assert!(
            paths
                .iter()
                .any(|path| path.path == "hasOffshoreWindTurbine"),
            "expected farm -> turbine relation path, got {paths:?}"
        );
    }

    #[test]
    fn capability_graph_exposes_time_and_numeric_fields() {
        let (_schema, graph) = graph();
        let weather = graph
            .root("queryWeatherPrediction")
            .expect("expected weather prediction root");

        assert!(weather.time_fields.contains("time"));
        assert!(weather.numeric_fields.contains("windSpeed10m"));
        assert!(weather.numeric_fields.contains("windGust80m"));
    }

    #[test]
    fn capability_refinement_keeps_time_numeric_root_for_trend_query() {
        let (schema, graph) = graph();
        let raw_slice = schema.schema_retrieval_slice("Show wind speed trend over time", 6, 6);
        let refined = graph.refine_retrieval_slice(raw_slice, "Show wind speed trend over time", 3);

        assert_eq!(
            refined.roots.first().map(|root| root.root.as_str()),
            Some("queryWeatherPrediction"),
            "expected weather prediction to be top-ranked after capability refinement, got {:?}",
            refined.roots
        );
    }

    #[test]
    fn capability_refinement_promotes_numeric_root_for_rank_query() {
        let (schema, graph) = graph();
        let raw_slice = schema.schema_retrieval_slice("Top 3 wind farms by ratedCapacity", 6, 6);
        let refined =
            graph.refine_retrieval_slice(raw_slice, "Top 3 wind farms by ratedCapacity", 3);

        assert_eq!(
            refined.roots.first().map(|root| root.root.as_str()),
            Some("queryOffshoreWindFarm"),
            "expected wind farm root to be top-ranked for ratedCapacity rank query, got {:?}",
            refined.roots
        );
    }

    #[test]
    fn generated_concept_aliases_are_low_trust_recall_evidence() {
        let (schema, graph) = graph();
        let raw_slice = schema.schema_retrieval_slice("List offshore wind turbine records", 6, 6);
        let refined =
            graph.refine_retrieval_slice(raw_slice, "List offshore wind turbine records", 3);
        let turbine = refined
            .roots
            .iter()
            .find(|root| root.root == "queryOffshoreWindTurbine")
            .expect("expected turbine root in refined slice");

        assert!(
            turbine
                .capability_evidence
                .iter()
                .any(|evidence| evidence.starts_with("generated_concept_recall:")),
            "expected generated type-name aliases to be visible as recall evidence: {turbine:?}"
        );
        assert!(
            turbine
                .capability_evidence
                .iter()
                .all(|evidence| !evidence.starts_with("sls_concept_match:offshore wind turbine")),
            "generated type-name aliases should not masquerade as trusted SLS concept evidence: {turbine:?}"
        );
    }

    #[test]
    fn capability_refinement_filters_rank_queries_to_numeric_roots_when_possible() {
        let (schema, graph) = graph();
        let raw_slice = schema.schema_retrieval_slice("Highest wind speed", 8, 6);
        let refined = graph.refine_retrieval_slice(raw_slice, "Highest wind speed", 5);

        assert!(
            refined.roots.iter().all(|root| {
                graph
                    .root(&root.root)
                    .is_some_and(|capability| !capability.numeric_fields.is_empty())
            }),
            "expected rank-like query roots to be numeric-capable when any numeric roots are available, got {:?}",
            refined.roots
        );
    }

    #[test]
    fn task_shape_does_not_carry_code_hardcoded_metric_words() {
        let vocabulary = crate::sls::IntentVocabulary {
            compare: vec!["compare".to_string()],
            ..crate::sls::IntentVocabulary::default()
        };
        let task = super::CapabilityTaskShape::from_tokens(
            &super::query_tokens("compare power"),
            &vocabulary,
        );

        assert!(
            task.compare_like,
            "expected SLS compare vocabulary to mark compare shape"
        );
        assert!(
            !task.numeric_metric_requested,
            "metric words should be grounded through SLS/schema, not hardcoded task-shape tokens"
        );
    }

    #[test]
    fn task_shape_uses_sls_group_by_vocabulary() {
        let vocabulary = crate::sls::IntentVocabulary {
            group_by: vec!["per".to_string()],
            ..crate::sls::IntentVocabulary::default()
        };
        let task = super::CapabilityTaskShape::from_tokens(
            &super::query_tokens("alarm count per turbine"),
            &vocabulary,
        );

        assert!(
            task.aggregate_like,
            "expected grouping terms to come from SLS intent vocabulary"
        );

        let without_sls_term = super::CapabilityTaskShape::from_tokens(
            &super::query_tokens("alarm count per turbine"),
            &crate::sls::IntentVocabulary::default(),
        );
        assert!(
            !without_sls_term.aggregate_like,
            "did not expect `per` to be hardcoded as an aggregate signal"
        );
    }

    #[test]
    fn capability_refinement_caps_schema_recall_before_sls_ranking() {
        let (_schema, graph) = graph();
        let raw_slice = SchemaRetrievalSlice {
            intent: "lookup/list".to_string(),
            profile: QueryRootRetrievalProfile {
                matches: Vec::new(),
                top_score: 5_000,
                runner_up_score: 1,
                competitive_root_count: 1,
                confidence: RetrievalConfidence::High,
            },
            roots: vec![
                root_slice("queryWeatherPrediction", "WeatherPrediction", 5_000),
                root_slice("queryPowerPrediction", "PowerPrediction", 1),
            ],
        };

        let refined = graph.refine_retrieval_slice(raw_slice, "forecast power", 2);

        assert_eq!(
            refined.roots.first().map(|root| root.root.as_str()),
            Some("queryPowerPrediction"),
            "expected SLS metric/source evidence to beat a huge raw token-recall score, got {:?}",
            refined.roots
        );
        assert!(
            refined
                .roots
                .iter()
                .any(|root| root.score >= super::ROOT_RECALL_SCORE_CEILING),
            "expected refined scores to retain a bounded recall contribution"
        );
    }

    #[test]
    fn capability_refinement_grounds_numeric_metric_request_from_sls_metrics() {
        let (schema, graph) = graph();
        let query = "Compare forecast power between locations";
        let raw_slice = schema.schema_retrieval_slice(query, 8, 6);
        let refined = graph.refine_retrieval_slice(raw_slice, query, 4);

        assert_eq!(
            refined.roots.first().map(|root| root.root.as_str()),
            Some("queryPowerPrediction"),
            "expected SLS forecast_power metric to promote power prediction, got {:?}",
            refined.roots
        );
        assert!(
            refined.roots.first().is_some_and(|root| root
                .capability_evidence
                .iter()
                .any(|evidence| evidence == "sls_metric_match:forecast_power")),
            "expected metric provenance from SLS, got {:?}",
            refined.roots.first()
        );
    }

    #[test]
    fn capability_refinement_uses_sls_canonical_field_defaults() {
        let (schema, graph) = graph();
        let raw_slice = schema.schema_retrieval_slice("Show weather prediction wind speed", 8, 6);
        let refined =
            graph.refine_retrieval_slice(raw_slice, "Show weather prediction wind speed", 3);

        assert_eq!(
            refined.roots.first().map(|root| root.root.as_str()),
            Some("queryWeatherPrediction"),
            "expected SLS `wind speed -> windSpeed10m` to promote weather prediction, got {:?}",
            refined.roots
        );
        assert!(
            refined
                .roots
                .first()
                .is_some_and(|root| root.capability_evidence.iter().any(|evidence| {
                    evidence.contains("sls_field_default_match")
                        || evidence.contains("sls_metric_match")
                })),
            "expected SLS provenance for weather metric/default boost, got {:?}",
            refined.roots.first()
        );
    }

    #[test]
    fn capability_refinement_uses_sls_preferred_join_for_membership_query() {
        let (schema, graph) = graph();
        let query = "List turbines in wind farm Wind Farm 3";
        let raw_slice = schema.schema_retrieval_slice(query, 6, 8);
        let refined = graph.refine_retrieval_slice(raw_slice, query, 4);
        let roots = refined
            .roots
            .iter()
            .map(|root| root.root.as_str())
            .collect::<Vec<_>>();

        assert!(
            roots.contains(&"queryOffshoreWindFarm") && roots.contains(&"queryOffshoreWindTurbine"),
            "expected SLS preferred join endpoints for farm-scoped turbine query, got {:?}",
            refined.roots
        );
        assert!(
            refined.roots.iter().all(|root| {
                matches!(
                    root.root.as_str(),
                    "queryOffshoreWindFarm" | "queryOffshoreWindTurbine"
                )
            }),
            "expected preferred join evidence to filter retrieval to join endpoints, got {:?}",
            refined.roots
        );
        assert!(
            refined.roots.iter().all(|root| root
                .capability_evidence
                .iter()
                .any(|evidence| evidence.contains("sls_preferred_join_match"))),
            "expected preferred join provenance on both roots, got {:?}",
            refined.roots
        );
    }

    #[test]
    fn capability_refinement_uses_sls_preferred_join_for_substation_membership_query() {
        let (schema, graph) = graph();
        let query = r#"List offshore substations for wind farm "Wind Farm 3"."#;
        let raw_slice = schema.schema_retrieval_slice(query, 6, 8);
        let refined = graph.refine_retrieval_slice(raw_slice, query, 4);
        let roots = refined
            .roots
            .iter()
            .map(|root| root.root.as_str())
            .collect::<Vec<_>>();

        assert!(
            roots.contains(&"queryOffshoreWindFarm") && roots.contains(&"queryOffshoreSubstation"),
            "expected SLS preferred join endpoints for farm-scoped substation query, got {:?}",
            refined.roots
        );
        assert!(
            refined.roots.iter().all(|root| root
                .capability_evidence
                .iter()
                .any(|evidence| evidence.contains("sls_preferred_join_match"))),
            "expected preferred join provenance on both roots, got {:?}",
            refined.roots
        );
    }
}
