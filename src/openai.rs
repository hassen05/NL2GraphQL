use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;

#[derive(Debug, Deserialize, Clone, ToSchema)]
#[allow(dead_code)] // OpenAI API compatibility - fields may be unused
pub struct ChatCompletionRequest {
    #[schema(example = "zephyr-agent")]
    pub model: String,
    pub messages: Vec<Message>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub stream: Option<bool>,
    pub dry_run: Option<bool>,
    pub execute: Option<bool>,
}

#[derive(Deserialize, Serialize, Clone, ToSchema)]
pub struct Message {
    #[schema(example = "user")]
    pub role: String,
    #[schema(example = "Hello, what is the status of turbine A?")]
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl std::fmt::Debug for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let content_summary = if self.content.len() > 100 {
            format!(
                "{}... (truncated, total {} chars)",
                &self.content[..100],
                self.content.len()
            )
        } else {
            self.content.clone()
        };

        f.debug_struct("Message")
            .field("role", &self.role)
            .field("content", &content_summary)
            .field("name", &self.name)
            .finish()
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ChatCompletionResponse {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<Choice>,
    pub usage: Usage,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct Choice {
    pub index: u32,
    pub message: Message,
    pub finish_reason: String,
}

#[derive(Debug, Serialize, ToSchema)]
#[allow(clippy::struct_field_names)] // OpenAI API compatibility
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct GraphQLToolRequest {
    /// GraphQL query document
    #[schema(example = "query { turbines { id name } }")]
    pub query: String,
    /// GraphQL variables object
    #[serde(default)]
    pub variables: Value,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct GraphQLToolResponse {
    /// GraphQL "data" field (if any)
    pub data: Option<Value>,
    /// GraphQL "errors" field (if any)
    pub errors: Option<Value>,
}

#[derive(Debug, Serialize)]
pub struct ChatCompletionChunk {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<ChoiceChunk>,
}

#[derive(Debug, Serialize)]
pub struct ChoiceChunk {
    pub index: u32,
    pub delta: Delta,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Delta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}
