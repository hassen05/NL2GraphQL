use crate::AppState;
use crate::service::{
    AskZephyrRequest, DirectGraphqlRequest, ExecutePlanRequest, HistoryRequest,
    InspectSchemaRequest, ZephyrService,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{debug, info, warn};

const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

#[derive(Clone, Debug, Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Option<Value>,
}

#[derive(Clone, Debug, Serialize)]
struct JsonRpcError {
    code: i64,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

#[derive(Clone, Debug, Deserialize)]
struct ToolCallParams {
    name: String,
    #[serde(default)]
    arguments: Value,
}

#[derive(Clone, Debug, Deserialize)]
struct ResourceReadParams {
    uri: String,
}

pub(crate) fn mcp_enabled(config: &crate::Config) -> bool {
    parse_bool(&config.zephyr_mcp_enabled)
}

pub(crate) fn http_enabled(config: &crate::Config) -> bool {
    parse_bool(&config.zephyr_http_enabled)
}

pub(crate) fn debug_tools_enabled(config: &crate::Config) -> bool {
    parse_bool(&config.mcp_debug_tools_enabled)
}

pub(crate) fn uses_stdio_transport(config: &crate::Config) -> bool {
    config.mcp_transport.trim().eq_ignore_ascii_case("stdio")
}

pub(crate) fn parse_bool(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

pub(crate) async fn run_stdio_server(state: Arc<AppState>) -> anyhow::Result<()> {
    if !uses_stdio_transport(&state.config) {
        anyhow::bail!(
            "unsupported MCP_TRANSPORT='{}'; v1 supports MCP_TRANSPORT=stdio only",
            state.config.mcp_transport
        );
    }

    info!("Starting Zephyr MCP stdio server.");
    let service = ZephyrService::new(state.clone());
    let debug_tools = debug_tools_enabled(&state.config);
    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();
    let mut stdout = tokio::io::stdout();

    while let Some(line) = lines.next_line().await? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        debug!("MCP request: {}", trimmed);
        if let Some(response) = handle_json_rpc_line(&service, debug_tools, trimmed).await {
            stdout.write_all(response.as_bytes()).await?;
            stdout.write_all(b"\n").await?;
            stdout.flush().await?;
        }
    }

    Ok(())
}

async fn handle_json_rpc_line(
    service: &ZephyrService,
    debug_tools: bool,
    line: &str,
) -> Option<String> {
    let request = match serde_json::from_str::<JsonRpcRequest>(line) {
        Ok(request) => request,
        Err(e) => {
            return Some(json_rpc_error(
                Value::Null,
                -32700,
                "Parse error",
                Some(json!({ "detail": e.to_string() })),
            ));
        }
    };

    let id = request.id.clone();
    let response = dispatch_request(service, debug_tools, request).await;
    id.map(|id| match response {
        Ok(result) => json_rpc_result(id, result),
        Err(error) => json_rpc_error_value(id, error),
    })
}

async fn dispatch_request(
    service: &ZephyrService,
    debug_tools: bool,
    request: JsonRpcRequest,
) -> Result<Value, JsonRpcError> {
    match request.method.as_str() {
        "initialize" => Ok(initialize_result()),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_specs(debug_tools) })),
        "tools/call" => {
            let params: ToolCallParams = from_params(request.params)?;
            call_tool(service, debug_tools, params).await
        }
        "resources/list" => Ok(json!({ "resources": resource_specs() })),
        "resources/read" => {
            let params: ResourceReadParams = from_params(request.params)?;
            read_resource(service, params).await
        }
        "notifications/initialized" => Ok(json!({})),
        other => Err(JsonRpcError {
            code: -32601,
            message: format!("Method not found: {other}"),
            data: None,
        }),
    }
}

async fn call_tool(
    service: &ZephyrService,
    debug_tools: bool,
    params: ToolCallParams,
) -> Result<Value, JsonRpcError> {
    let arguments = normalize_arguments(params.arguments);
    match params.name.as_str() {
        "ask_zephyr" => {
            let req: AskZephyrRequest = decode_arguments(arguments)?;
            if req.include_debug && !debug_tools {
                return Err(debug_disabled_error("ask_zephyr.include_debug"));
            }
            let response = service.ask(req).await.map_err(pipeline_error)?;
            Ok(tool_json_result(&response))
        }
        "inspect_schema" => {
            let req: InspectSchemaRequest = decode_arguments(arguments)?;
            let response = service.inspect_schema(req).await;
            Ok(tool_json_result(&response))
        }
        "get_history" => {
            let req: HistoryRequest = decode_arguments(arguments)?;
            if req.include_debug && !debug_tools {
                return Err(debug_disabled_error("get_history.include_debug"));
            }
            let response = service.history(req).await;
            Ok(tool_json_result(&response))
        }
        "plan_query" => {
            require_debug_tool(debug_tools, "plan_query")?;
            let req: AskZephyrRequest = decode_arguments(arguments)?;
            let response = service
                .plan_query(&req.question)
                .await
                .map_err(pipeline_error)?;
            Ok(tool_json_result(&response))
        }
        "direct_graphql_query" => {
            require_debug_tool(debug_tools, "direct_graphql_query")?;
            let req: DirectGraphqlRequest = decode_arguments(arguments)?;
            let response = service
                .direct_graphql(req, debug_tools)
                .await
                .map_err(|message| JsonRpcError {
                    code: -32010,
                    message,
                    data: None,
                })?;
            Ok(tool_json_result(&response))
        }
        "execute_plan" => {
            require_debug_tool(debug_tools, "execute_plan")?;
            let req: ExecutePlanRequest = decode_arguments(arguments)?;
            let response = service.execute_plan(req).await.map_err(pipeline_error)?;
            Ok(tool_json_result(&response))
        }
        other => Err(JsonRpcError {
            code: -32602,
            message: format!("Unknown tool: {other}"),
            data: None,
        }),
    }
}

async fn read_resource(
    service: &ZephyrService,
    params: ResourceReadParams,
) -> Result<Value, JsonRpcError> {
    let (mime_type, value) = match params.uri.as_str() {
        "zephyr://schema/summary" => (
            "application/json",
            service
                .inspect_schema(InspectSchemaRequest {
                    question: None,
                    limit: Some(8),
                })
                .await,
        ),
        "zephyr://sls/summary" => ("application/json", service.sls_summary().await),
        "zephyr://history/recent" => (
            "application/json",
            service
                .history(HistoryRequest {
                    limit: Some(25),
                    search: None,
                    include_debug: false,
                })
                .await,
        ),
        "zephyr://config/runtime" => ("application/json", service.runtime_summary().await),
        other => {
            return Err(JsonRpcError {
                code: -32602,
                message: format!("Unknown resource URI: {other}"),
                data: None,
            });
        }
    };
    Ok(json!({
        "contents": [{
            "uri": params.uri,
            "mimeType": mime_type,
            "text": serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string())
        }]
    }))
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": MCP_PROTOCOL_VERSION,
        "capabilities": {
            "tools": {},
            "resources": {}
        },
        "serverInfo": {
            "name": "zephyr-agent",
            "version": env!("CARGO_PKG_VERSION")
        }
    })
}

fn tool_specs(debug_tools: bool) -> Vec<Value> {
    let mut tools = vec![
        json!({
            "name": "ask_zephyr",
            "description": "Ask Zephyr a natural-language question and get a grounded answer from the existing PlanV2 pipeline.",
            "inputSchema": {
                "type": "object",
                "required": ["question"],
                "properties": {
                    "question": { "type": "string" },
                    "execute": { "type": "boolean", "default": true },
                    "include_debug": { "type": "boolean", "default": false },
                    "model": { "type": "string" }
                }
            }
        }),
        json!({
            "name": "inspect_schema",
            "description": "Inspect the compact schema/SLS retrieval slice Zephyr would use for a question.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "question": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 12 }
                }
            }
        }),
        json!({
            "name": "get_history",
            "description": "Read safe recent Zephyr query history.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100 },
                    "search": { "type": "string" },
                    "include_debug": { "type": "boolean", "default": false }
                }
            }
        }),
    ];
    if debug_tools {
        tools.extend([
            json!({
                "name": "plan_query",
                "description": "Run Zephyr planning and validation without backend execution.",
                "inputSchema": {
                    "type": "object",
                    "required": ["question"],
                    "properties": {
                        "question": { "type": "string" },
                        "model": { "type": "string" }
                    }
                }
            }),
            json!({
                "name": "direct_graphql_query",
                "description": "Run a validated direct GraphQL query through Zephyr's existing direct-query guard.",
                "inputSchema": {
                    "type": "object",
                    "required": ["query"],
                    "properties": {
                        "query": { "type": "string" },
                        "variables": { "type": "object" }
                    }
                }
            }),
            json!({
                "name": "execute_plan",
                "description": "Validate, compile, and execute a supplied PlanV2 object through Zephyr's executor.",
                "inputSchema": {
                    "type": "object",
                    "required": ["plan_v2"],
                    "properties": {
                        "plan_v2": { "type": "object" },
                        "question": { "type": "string" },
                        "model": { "type": "string" }
                    }
                }
            }),
        ]);
    }
    tools
}

fn resource_specs() -> Vec<Value> {
    vec![
        json!({
            "uri": "zephyr://schema/summary",
            "name": "Schema retrieval summary",
            "description": "Compact schema roots and field capabilities.",
            "mimeType": "application/json"
        }),
        json!({
            "uri": "zephyr://sls/summary",
            "name": "SLS summary",
            "description": "Semantic layer concept, metric, and role counts.",
            "mimeType": "application/json"
        }),
        json!({
            "uri": "zephyr://history/recent",
            "name": "Recent query history",
            "description": "Safe recent query history without debug output.",
            "mimeType": "application/json"
        }),
        json!({
            "uri": "zephyr://config/runtime",
            "name": "Runtime configuration",
            "description": "Non-secret runtime mode and provider summary.",
            "mimeType": "application/json"
        }),
    ]
}

fn normalize_arguments(arguments: Value) -> Value {
    if arguments.is_null() {
        json!({})
    } else {
        arguments
    }
}

fn from_params<T: for<'de> Deserialize<'de>>(params: Option<Value>) -> Result<T, JsonRpcError> {
    serde_json::from_value(params.unwrap_or_else(|| json!({}))).map_err(|e| JsonRpcError {
        code: -32602,
        message: format!("Invalid params: {e}"),
        data: None,
    })
}

fn decode_arguments<T: for<'de> Deserialize<'de>>(arguments: Value) -> Result<T, JsonRpcError> {
    serde_json::from_value(arguments).map_err(|e| JsonRpcError {
        code: -32602,
        message: format!("Invalid tool arguments: {e}"),
        data: None,
    })
}

fn require_debug_tool(debug_tools: bool, name: &str) -> Result<(), JsonRpcError> {
    if debug_tools {
        Ok(())
    } else {
        Err(debug_disabled_error(name))
    }
}

fn debug_disabled_error(name: &str) -> JsonRpcError {
    JsonRpcError {
        code: -32001,
        message: format!("{name} requires MCP_DEBUG_TOOLS_ENABLED=true"),
        data: None,
    }
}

fn pipeline_error(err: crate::error::PipelineError) -> JsonRpcError {
    JsonRpcError {
        code: -32000,
        message: err.to_string(),
        data: None,
    }
}

fn tool_json_result(value: &impl Serialize) -> Value {
    let text = serde_json::to_string_pretty(value).unwrap_or_else(|e| {
        warn!("Failed to serialize MCP tool result: {e}");
        "{}".to_string()
    });
    json!({
        "content": [{
            "type": "text",
            "text": text
        }]
    })
}

fn json_rpc_result(id: Value, result: Value) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result
    })
    .to_string()
}

fn json_rpc_error(id: Value, code: i64, message: &str, data: Option<Value>) -> String {
    json_rpc_error_value(
        id,
        JsonRpcError {
            code,
            message: message.to_string(),
            data,
        },
    )
}

fn json_rpc_error_value(id: Value, error: JsonRpcError) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": error
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bool_parser_accepts_common_true_values() {
        assert!(parse_bool("true"));
        assert!(parse_bool("1"));
        assert!(parse_bool("YES"));
        assert!(!parse_bool("false"));
        assert!(!parse_bool(""));
    }

    #[test]
    fn tool_specs_hide_debug_tools_by_default() {
        let names = tool_specs(false)
            .into_iter()
            .filter_map(|tool| tool.get("name").and_then(Value::as_str).map(str::to_string))
            .collect::<Vec<_>>();
        assert!(names.contains(&"ask_zephyr".to_string()));
        assert!(!names.contains(&"plan_query".to_string()));
        assert!(!names.contains(&"direct_graphql_query".to_string()));
    }

    #[test]
    fn tool_specs_include_debug_tools_when_enabled() {
        let names = tool_specs(true)
            .into_iter()
            .filter_map(|tool| tool.get("name").and_then(Value::as_str).map(str::to_string))
            .collect::<Vec<_>>();
        assert!(names.contains(&"plan_query".to_string()));
        assert!(names.contains(&"direct_graphql_query".to_string()));
        assert!(names.contains(&"execute_plan".to_string()));
    }

    #[test]
    fn debug_tool_guard_returns_mcp_error() {
        let err = require_debug_tool(false, "plan_query").expect_err("debug must be gated");
        assert_eq!(err.code, -32001);
        assert!(err.message.contains("MCP_DEBUG_TOOLS_ENABLED=true"));
    }

    #[test]
    fn initialize_result_uses_mcp_shape() {
        let result = initialize_result();
        assert_eq!(result["protocolVersion"], MCP_PROTOCOL_VERSION);
        assert_eq!(result["serverInfo"]["name"], "zephyr-agent");
    }
}
