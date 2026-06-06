/// Query history tracking for conversation context and audit trail
///
/// This module provides in-memory storage of user questions and system responses
/// with timestamps and execution metadata for debugging and user reference.
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use uuid::Uuid;

/// A single entry in the conversation history
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// Unique identifier for this query
    pub id: String,
    /// The user's natural language question
    pub question: String,
    /// The system's final answer
    pub answer: Option<String>,
    /// Structured plan generated (PlanV2 JSON as string)
    pub plan: Option<String>,
    /// Execution time in milliseconds
    pub execution_ms: u64,
    /// Whether the query executed successfully
    pub success: bool,
    /// Error message if failed
    pub error: Option<String>,
    /// When this query was made
    pub timestamp: String,
}

impl HistoryEntry {
    /// Create a new history entry
    pub fn new(question: String) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            question,
            answer: None,
            plan: None,
            execution_ms: 0,
            success: false,
            error: None,
            timestamp: Utc::now().to_rfc3339(),
        }
    }
}

/// In-memory conversation history storage with bounded capacity
///
/// The history is stored as a FIFO queue with a maximum size.
/// Older entries are automatically removed when capacity is exceeded.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QueryHistory {
    /// Maximum number of entries to keep in history
    max_entries: usize,
    /// FIFO queue of history entries (newest at the back)
    entries: VecDeque<HistoryEntry>,
}

impl QueryHistory {
    /// Create a new history tracker with specified capacity
    pub fn new(max_entries: usize) -> Self {
        Self {
            max_entries: if max_entries == 0 { 100 } else { max_entries },
            entries: VecDeque::new(),
        }
    }

    /// Add a new entry to the history
    ///
    /// If at capacity, the oldest entry is removed
    pub fn add(&mut self, entry: HistoryEntry) {
        if self.entries.len() >= self.max_entries {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
    }

    /// Get the last N entries (most recent first)
    pub fn get_recent(&self, limit: usize) -> Vec<HistoryEntry> {
        self.entries.iter().rev().take(limit).cloned().collect()
    }

    /// Clear all history
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Get the number of entries currently stored
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Get entries matching a search term (searches in questions and answers)
    pub fn search(&self, term: &str) -> Vec<HistoryEntry> {
        let term_lower = term.to_lowercase();
        self.entries
            .iter()
            .filter(|e| {
                e.question.to_lowercase().contains(&term_lower)
                    || e.answer
                        .as_ref()
                        .map(|a| a.to_lowercase().contains(&term_lower))
                        .unwrap_or(false)
            })
            .cloned()
            .collect()
    }

    /// Get summary statistics about history
    pub fn stats(&self) -> HistoryStats {
        let total = self.entries.len();
        let successful = self.entries.iter().filter(|e| e.success).count();
        let failed = total - successful;
        let avg_time = if total > 0 {
            self.entries.iter().map(|e| e.execution_ms).sum::<u64>() / total as u64
        } else {
            0
        };

        HistoryStats {
            total_queries: total,
            successful_queries: successful,
            failed_queries: failed,
            average_execution_ms: avg_time,
            capacity: self.max_entries,
        }
    }

    /// Load history from a JSON string, falling back to an empty history on parse failure.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        let mut history: Self = serde_json::from_str(json)?;
        if history.max_entries == 0 {
            history.max_entries = 100;
        }
        Ok(history)
    }

    /// Render history as a pretty JSON document for persistence.
    pub fn to_json_pretty(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

/// Summary statistics about the query history
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryStats {
    pub total_queries: usize,
    pub successful_queries: usize,
    pub failed_queries: usize,
    pub average_execution_ms: u64,
    pub capacity: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_history_add_and_get() {
        let mut history = QueryHistory::new(10);
        let entry = HistoryEntry::new("What is the answer?".to_string());

        history.add(entry);
        assert_eq!(history.len(), 1);
        assert_eq!(history.get_recent(1)[0].question, "What is the answer?");
    }

    #[test]
    fn test_history_capacity() {
        let mut history = QueryHistory::new(3);
        for i in 0..5 {
            let mut entry = HistoryEntry::new(format!("Query {}", i));
            entry.success = true;
            history.add(entry);
        }
        assert_eq!(history.len(), 3);
    }

    #[test]
    fn test_history_search() {
        let mut history = QueryHistory::new(10);
        let mut entry1 = HistoryEntry::new("Find turbines".to_string());
        entry1.answer = Some("Found 5 turbines".to_string());
        let mut entry2 = HistoryEntry::new("What about cables?".to_string());
        entry2.answer = Some("No cables found".to_string());

        history.add(entry1);
        history.add(entry2);

        let results = history.search("turbine");
        assert_eq!(results.len(), 1);
        assert!(results[0].question.contains("turbine"));
    }

    #[test]
    fn test_history_stats() {
        let mut history = QueryHistory::new(10);
        for i in 0..3 {
            let mut entry = HistoryEntry::new(format!("Query {}", i));
            entry.execution_ms = 100;
            entry.success = i < 2; // First 2 succeed
            history.add(entry);
        }

        let stats = history.stats();
        assert_eq!(stats.total_queries, 3);
        assert_eq!(stats.successful_queries, 2);
        assert_eq!(stats.failed_queries, 1);
        assert_eq!(stats.average_execution_ms, 100);
    }

    #[test]
    fn test_history_round_trip_json() {
        let mut history = QueryHistory::new(5);
        let mut entry = HistoryEntry::new("Persist me".to_string());
        entry.answer = Some("Done".to_string());
        history.add(entry);

        let json = history.to_json_pretty().expect("serialize history");
        let loaded = QueryHistory::from_json(&json).expect("deserialize history");

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded.get_recent(1)[0].question, "Persist me");
    }
}
