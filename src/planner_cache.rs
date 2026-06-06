use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};

use crate::entity_linker::EntityResolution;

const DEFAULT_MAX_CONTEXT_ENTRIES: usize = 256;
const DEFAULT_MAX_RESPONSE_ENTRIES: usize = 128;
const DEFAULT_MAX_GROUNDING_ENTRIES: usize = 512;
const DEFAULT_MAX_GROUNDING_QUESTION_ENTRIES: usize = 256;

#[derive(Clone, Debug)]
pub(crate) struct StaticPromptHints {
    pub(crate) root_fields: String,
    pub(crate) policy_hints: String,
    pub(crate) join_hints: String,
    pub(crate) metric_hints: String,
    pub(crate) field_hints: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub(crate) struct PlannerContextCacheKey {
    schema_version: String,
    query: String,
    root_limit: usize,
    field_limit: usize,
    anchored_roots: Vec<String>,
}

impl PlannerContextCacheKey {
    pub(crate) fn new(
        schema_version: impl Into<String>,
        query: impl Into<String>,
        root_limit: usize,
        field_limit: usize,
        anchored_roots: Vec<String>,
    ) -> Self {
        Self {
            schema_version: schema_version.into(),
            query: query.into(),
            root_limit,
            field_limit,
            anchored_roots,
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub(crate) struct PlannerResponseCacheKey {
    schema_version: String,
    model_name: String,
    prompt_hash: u64,
}

impl PlannerResponseCacheKey {
    pub(crate) fn new(
        schema_version: impl Into<String>,
        model_name: impl Into<String>,
        prompt: &str,
    ) -> Self {
        Self {
            schema_version: schema_version.into(),
            model_name: model_name.into(),
            prompt_hash: stable_hash(prompt),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PlannerContextCacheEntry {
    pub(crate) schema_snippet: String,
    pub(crate) preferred_root_fields: Vec<String>,
    pub(crate) schema_retrieval: serde_json::Value,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub(crate) struct GroundingCacheKey {
    schema_version: String,
    mention: String,
    compact_identifier: bool,
    limit: usize,
}

impl GroundingCacheKey {
    pub(crate) fn new(
        schema_version: impl Into<String>,
        mention: impl Into<String>,
        compact_identifier: bool,
        limit: usize,
    ) -> Self {
        Self {
            schema_version: schema_version.into(),
            mention: mention.into(),
            compact_identifier,
            limit,
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub(crate) struct GroundingQuestionCacheKey {
    schema_version: String,
    normalized_question: String,
    limit: usize,
}

impl GroundingQuestionCacheKey {
    pub(crate) fn new(
        schema_version: impl Into<String>,
        normalized_question: impl Into<String>,
        limit: usize,
    ) -> Self {
        Self {
            schema_version: schema_version.into(),
            normalized_question: normalized_question.into(),
            limit,
        }
    }
}

#[derive(Debug)]
pub(crate) struct PlannerPromptCache {
    max_context_entries: usize,
    max_response_entries: usize,
    max_grounding_entries: usize,
    max_grounding_question_entries: usize,
    static_hints: Option<(String, StaticPromptHints)>,
    context_entries: HashMap<PlannerContextCacheKey, PlannerContextCacheEntry>,
    planner_responses: HashMap<PlannerResponseCacheKey, String>,
    grounding_entries: HashMap<GroundingCacheKey, EntityResolution>,
    grounding_question_entries: HashMap<GroundingQuestionCacheKey, Vec<EntityResolution>>,
    grounding_question_order: VecDeque<GroundingQuestionCacheKey>,
}

impl Default for PlannerPromptCache {
    fn default() -> Self {
        Self {
            max_context_entries: DEFAULT_MAX_CONTEXT_ENTRIES,
            max_response_entries: DEFAULT_MAX_RESPONSE_ENTRIES,
            max_grounding_entries: DEFAULT_MAX_GROUNDING_ENTRIES,
            max_grounding_question_entries: DEFAULT_MAX_GROUNDING_QUESTION_ENTRIES,
            static_hints: None,
            context_entries: HashMap::new(),
            planner_responses: HashMap::new(),
            grounding_entries: HashMap::new(),
            grounding_question_entries: HashMap::new(),
            grounding_question_order: VecDeque::new(),
        }
    }
}

impl PlannerPromptCache {
    pub(crate) fn static_hints(&self, schema_version: &str) -> Option<StaticPromptHints> {
        self.static_hints
            .as_ref()
            .filter(|(version, _)| version == schema_version)
            .map(|(_, hints)| hints.clone())
    }

    pub(crate) fn set_static_hints(
        &mut self,
        schema_version: impl Into<String>,
        hints: StaticPromptHints,
    ) {
        self.static_hints = Some((schema_version.into(), hints));
    }

    pub(crate) fn context_entry(
        &self,
        key: &PlannerContextCacheKey,
    ) -> Option<PlannerContextCacheEntry> {
        self.context_entries.get(key).cloned()
    }

    pub(crate) fn insert_context_entry(
        &mut self,
        key: PlannerContextCacheKey,
        entry: PlannerContextCacheEntry,
    ) {
        if self.context_entries.len() >= self.max_context_entries
            && let Some(oldest_key) = self.context_entries.keys().next().cloned()
        {
            self.context_entries.remove(&oldest_key);
        }
        self.context_entries.insert(key, entry);
    }

    pub(crate) fn planner_response(&self, key: &PlannerResponseCacheKey) -> Option<String> {
        self.planner_responses.get(key).cloned()
    }

    pub(crate) fn insert_planner_response(
        &mut self,
        key: PlannerResponseCacheKey,
        response: String,
    ) {
        if self.planner_responses.len() >= self.max_response_entries
            && let Some(evicted_key) = self.planner_responses.keys().next().cloned()
        {
            self.planner_responses.remove(&evicted_key);
        }
        self.planner_responses.insert(key, response);
    }

    pub(crate) fn grounding_entry(&self, key: &GroundingCacheKey) -> Option<EntityResolution> {
        self.grounding_entries.get(key).cloned()
    }

    pub(crate) fn insert_grounding_entry(
        &mut self,
        key: GroundingCacheKey,
        resolution: EntityResolution,
    ) {
        if self.grounding_entries.len() >= self.max_grounding_entries
            && let Some(evicted_key) = self.grounding_entries.keys().next().cloned()
        {
            self.grounding_entries.remove(&evicted_key);
        }
        self.grounding_entries.insert(key, resolution);
    }

    pub(crate) fn grounding_question_entry(
        &mut self,
        key: &GroundingQuestionCacheKey,
    ) -> Option<Vec<EntityResolution>> {
        let entry = self.grounding_question_entries.get(key).cloned();
        if entry.is_some() {
            self.touch_grounding_question_key(key);
        }
        entry
    }

    pub(crate) fn insert_grounding_question_entry(
        &mut self,
        key: GroundingQuestionCacheKey,
        resolutions: Vec<EntityResolution>,
    ) {
        if self.grounding_question_entries.contains_key(&key) {
            self.touch_grounding_question_key(&key);
            self.grounding_question_entries.insert(key, resolutions);
            return;
        }
        while self.grounding_question_entries.len() >= self.max_grounding_question_entries {
            let Some(evicted_key) = self.grounding_question_order.pop_front() else {
                break;
            };
            self.grounding_question_entries.remove(&evicted_key);
        }
        self.grounding_question_order.push_back(key.clone());
        self.grounding_question_entries.insert(key, resolutions);
    }

    pub(crate) fn clear(&mut self) {
        self.static_hints = None;
        self.context_entries.clear();
        self.planner_responses.clear();
        self.grounding_entries.clear();
        self.grounding_question_entries.clear();
        self.grounding_question_order.clear();
    }

    fn touch_grounding_question_key(&mut self, key: &GroundingQuestionCacheKey) {
        self.grounding_question_order
            .retain(|existing| existing != key);
        self.grounding_question_order.push_back(key.clone());
    }
}

fn stable_hash(value: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}
