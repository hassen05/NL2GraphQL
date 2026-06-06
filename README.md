# Zephyr Agent

Zephyr Agent is a schema-grounded natural-language interface for GraphQL data.

Instead of asking an LLM to write GraphQL directly, it plans in a typed intermediate form called `PlanV2`, validates that plan against the active schema, compiles `fetch` steps deterministically into GraphQL, optionally executes them, and returns grounded answers. The core architectural claim is that direct `NL -> GraphQL` generation is the failure mode to avoid: it forces the model to choose user intent, schema path, and exact query syntax all at once, which makes semantic drift and schema hallucinations much harder to control.

## What It Does
- accepts a natural-language question
- produces a typed `PlanV2` plan
- validates plan structure against schema-derived constraints
- compiles `fetch` steps to GraphQL deterministically
- executes queries and applies deterministic operators such as `aggregate`, `filter_rows`, `rank`, `compare`, `distance_haversine`, and `trend_summary`
- returns grounded natural-language answers
- exposes debug output with plan, executed queries, scope, and repair traces when requested

## Quick Start
1. Start your model backend.
   - default provider: `LLM_PROVIDER=ollama`
   - default Ollama URL: `OLLAMA_URL=http://localhost:11434`
   - default Ollama model: `OLLAMA_MODEL=gpt-oss:120b-cloud`
2. Start the agent for normal public use:

```bash
cargo run
```

3. Open `http://localhost:8080/`

The default page is the redesigned Zephyr Workspace. Anonymous users can ask
questions, see grounded answers, structured result views, charts when structured
evidence is available, and query history.

To unlock admin-only debug tools, start the server with an admin account:

```bash
ADMIN_USERNAME=admin \
ADMIN_PASSWORD='admin123' \
SESSION_SECRET='tjdydytj6uegueu5656ueg' \
cargo run
```

Then open `http://localhost:8080/` and use the admin login form. Admin mode
unlocks provenance, generated GraphQL, full debug output, the eval dashboard,
and other debug/admin controls.

## Private Backend Setup
To test against a private GraphQL backend:

1. Set backend access
   - `GRAPH_ENDPOINT`
   - `GRAPH_BEARER_TOKEN` or `GRAPH_API_KEY_HEADER` / `GRAPH_API_KEY` if required
2. Optionally provide a local schema file
   - `SCHEMA_FILE_PATH=/path/to/schema.graphql`
   - or `SCHEMA_FILE_PATH=/path/to/introspection.json`
3. Run the server with admin auth enabled
4. Log in as admin from `/`
5. Open `/config` and confirm:
   - `schema_source`
   - `schema_file_path`
   - `schema_cache_path`
   - `schema_cache_stale`

Schema source priority is:
1. `SCHEMA_FILE_PATH` if set
2. live GraphQL introspection from `GRAPH_ENDPOINT`
3. cached introspection from `SCHEMA_CACHE_PATH`
4. bundled fallback schema `schemas/consumer_schema.graphql`

Notes:
- `SCHEMA_FILE_PATH` supports either SDL or introspection JSON
- execution still uses `GRAPH_ENDPOINT`; the schema file only controls runtime schema metadata
- the bundled schema is a last-resort development fallback

## Configuration
`config.toml` supports env overrides for the main runtime knobs:

- `LLM_PROVIDER`
- `OLLAMA_URL`
- `OLLAMA_MODEL`
- `OPENAI_API_KEY`
- `OPENAI_MODEL`
- `OPENAI_BASE_URL`
- `ANTHROPIC_API_KEY`
- `ANTHROPIC_MODEL`
- `SERVER_HOST`
- `SERVER_PORT`
- `ADMIN_USERNAME`
- `ADMIN_PASSWORD`
- `SESSION_SECRET`
- `SESSION_TTL_HOURS`
- `SESSION_COOKIE_SECURE`
- `GRAPH_ENDPOINT`
- `GRAPH_BEARER_TOKEN`
- `GRAPH_API_KEY_HEADER`
- `GRAPH_API_KEY`
- `DIRECT_GRAPHQL_QUERY_ENABLED`
- `EXECUTE_ENABLED`
- `ZEPHYR_MCP_ENABLED`
- `ZEPHYR_HTTP_ENABLED`
- `MCP_TRANSPORT`
- `MCP_DEBUG_TOOLS_ENABLED`
- `SCHEMA_FILE_PATH`
- `SCHEMA_CACHE_PATH`
- `SCHEMA_CACHE_TTL_MINUTES`
- `SCHEMA_REFRESH_INTERVAL_MINUTES`

Provider examples:

```bash
LLM_PROVIDER=ollama \
OLLAMA_URL=http://localhost:11434 \
OLLAMA_MODEL=gpt-oss:120b-cloud \
cargo run
```

```bash
LLM_PROVIDER=openai \
OPENAI_API_KEY='...' \
OPENAI_MODEL=gpt-4o-mini \
cargo run
```

```bash
LLM_PROVIDER=anthropic \
ANTHROPIC_API_KEY='...' \
ANTHROPIC_MODEL=claude-3-5-sonnet-latest \
cargo run
```

PlanV2 planning, schema retrieval, validation, GraphQL execution, and answer grounding are provider-independent; only the LLM call layer changes.

Defaults currently live in [config.toml](config.toml).

If `ADMIN_PASSWORD` is set, `SESSION_SECRET` must also be set. Without a
session secret, admin login is disabled to avoid unsigned debug sessions.

## Request Modes
The chat endpoint supports two separate controls:

- `execute`
  - `false`: planning/debug flow only
  - `true`: execute against the backend and return grounded results
- `dry_run`
  - include debug/provenance-heavy output
  - this does not disable execution by itself
  - admin authentication is required

The direct `/graphql/query` proxy is disabled by default. Set `DIRECT_GRAPHQL_QUERY_ENABLED=true` only for trusted/debug deployments; enabled requests are still validated against the active schema and reject mutations, subscriptions, fragments, shorthand selection sets, variables, and introspection fields before execution.

## MCP Runtime
Zephyr can also run as a local stdio MCP server for agent clients. MCP is off by
default, so the normal web server behavior is unchanged.

Web only:

```bash
cargo run
```

MCP stdio only:

```bash
ZEPHYR_MCP_ENABLED=true \
ZEPHYR_HTTP_ENABLED=false \
cargo run
```

Web and MCP together:

```bash
ZEPHYR_MCP_ENABLED=true \
cargo run
```

Safe MCP tools are available by default:
- `ask_zephyr`
- `inspect_schema`
- `get_history`

Debug/admin MCP tools are hidden unless explicitly enabled:

```bash
ZEPHYR_MCP_ENABLED=true \
MCP_DEBUG_TOOLS_ENABLED=true \
cargo run
```

Debug MCP tools include `plan_query`, `direct_graphql_query`, and
`execute_plan`. For MCP, `direct_graphql_query` is available only when
`MCP_DEBUG_TOOLS_ENABLED=true`; it still passes through Zephyr's schema
validation guard. The HTTP `/graphql/query` proxy is separate and still requires
`DIRECT_GRAPHQL_QUERY_ENABLED=true`.

MCP execution uses the same backend endpoint as the web UI:

```bash
GRAPH_ENDPOINT=http://localhost:8000/graphql
```

Make sure this URL is reachable from the process that starts Zephyr. If the
GraphQL backend is only reachable from inside the Docker network, run Zephyr in
that same network or set `GRAPH_ENDPOINT` to the Docker service URL, for example:

```bash
GRAPH_ENDPOINT=http://zephyr-agent-thanos-1:8000/graphql \
ZEPHYR_MCP_ENABLED=true \
ZEPHYR_HTTP_ENABLED=false \
cargo run
```

If `ask_zephyr` works with `execute=false` but `ask_zephyr` with execution or
`direct_graphql_query` fails with `error sending request for url
(http://localhost:8000/graphql)`, MCP is healthy but the backend endpoint is not
reachable from Zephyr.

Example: planning only

```json
{
  "model": "",
  "execute": false,
  "messages": [
    { "role": "user", "content": "List the first 3 SCADA signals with tepId = TEP-123." }
  ]
}
```

Example: execute with debug output

First log in through `/auth/login` or use the browser admin login form so the
request carries the `zephyr_session` cookie.

```json
{
  "model": "",
  "execute": true,
  "dry_run": true,
  "messages": [
    { "role": "user", "content": "List the first 3 offshore wind farms." }
  ]
}
```

When `dry_run: true`, output may include:
- planner JSON
- effective executed queries
- `DEBUG_PREP_LOGS`
- `[QUERY_REPAIR_TRACE]` sections
- provenance and scope details
- timing / prompt-size metrics
- rough token estimates such as `planner_prompt_tokens_est`
- real provider token usage when the active backend returns it, for example `provider_token_usage.prompt_tokens`

The `*_tokens_est` fields are still rough estimates derived from prompt/response size, but provenance can now also carry real provider-billed usage through:
- `provider_token_usage`
- `provider_token_usage_available`

Whether real token usage appears depends on the active provider/backend:
- OpenAI/Copilot-style backends may expose usage directly
- Ollama-compatible backends may expose it through the OpenAI-compatible response shape
- cached planner/repair responses do not generate new provider usage for that step
- if the backend does not return usage, the agent falls back to estimates only

## UI
The browser UI at `/` serves the redesigned Zephyr Workspace.

Public users can:
- ask natural-language questions in execute mode
- see grounded answers
- inspect structured result cards/tables
- view charts when row-aligned structured evidence is available
- search recent query history
- export visible structured results as CSV, JSON, or Markdown

Admins can additionally:
- enable debug/provenance output
- inspect PlanV2 steps and generated GraphQL
- copy the full debug output
- open the eval dashboard
- access config/debug-only endpoints and admin controls

For exact lookups, quoted names such as `"Wind Farm 1"` or `"the wagon"` usually work better than unquoted plain text.

## Why `PlanV2`
Direct `NL -> GraphQL` generation tends to hallucinate fields, arguments, and unsafe query shapes.

It also mixes two different problems into one decoding step:
- deciding what operation the user actually wants,
- and spelling that operation correctly in backend query syntax.

That means a query can be syntactically valid while still expressing the wrong comparison, grouping, scope, or join path.

`PlanV2` is the typed planner contract between the LLM and the runtime:

```text
NL question -> PlanV2 -> validated fetch steps -> GraphQL -> optional execution
```

The division of responsibility is:
- LLM decides the high-level operation structure
- runtime validates the plan against the schema
- runtime compiles `fetch` steps deterministically
- runtime executes, repairs boundedly, and grounds the answer in evidence

`PlanV2` is the solution because it restores a clean semantic boundary:
- intent is explicit and inspectable before any GraphQL is emitted,
- operator shape can be validated before backend execution,
- `fetch` compilation is deterministic instead of prompt-dependent,
- repair stays bounded because it operates on a validated structure rather than a free-form query string,
- evaluation can distinguish planner failures from backend/schema limitations.

This makes the system easier to debug, safer to execute, and more maintainable under schema changes than direct GraphQL generation.

## Runtime Shape
```text
User
  -> LLM planner
  -> PlanV2 validation
  -> deterministic fetch compilation
  -> optional backend execution
  -> grounded answer
```

## Notes
- schema metadata is loaded from a configured local file, live introspection, cached introspection, or the bundled fallback schema
- the project follows a strict `PlanV2 -> validation -> deterministic compile/execute` path
- some question types still depend on backend schema completeness for full correctness
- backend relation gaps are surfaced as backend/schema issues where possible rather than hidden behind generic answers

## Evaluation Harnesses
Zephyr currently has a few different evaluation entry points, each useful for a different question.

### 1. Smoke run
Good for broad regression checks across a prompt set.

```bash
python3 run_smoke.py \
  --prompts docs/expected_user_questions_smoke_p3.txt \
  --out-json eval/results/smoke_q2_debug.json \
  --out-md eval/results/smoke_q2_debug.md \
  --debug \
  --timeout-sec 180 \
  --admin-username admin \
  --admin-password 'admin123'
```

### 2. DB-backed golden set
Good for semantic correctness checks against curated expected facts.

```bash
python3 eval/run_db_golden.py \
  --model gpt-oss:120b-cloud \
  --cases eval/db_golden_cases.jsonl \
  --out-json eval/results/db_golden_gpt120b.json \
  --out-md eval/results/db_golden_gpt120b.md \
  --debug \
  --timeout-sec 180
```

Debug golden runs require admin access on the running server. Use the same
admin startup command from Quick Start before running debug evals.

### 3. Model matrix
Good for comparing providers/models on latency, stability, and token usage.

```bash
python3 eval/run_model_matrix.py \
  --prompts docs/expected_user_questions_smoke_p3.txt \
  --models gpt-oss:20b-cloud gpt-oss:120b-cloud qwen3.5:397b-cloud deepseek-v3.1:671b-cloud \
  --out-dir eval/results/model_matrix \
  --debug
```

Notes:
- smoke success rates are executability / stability signals, not final denotation-accuracy scores
- backend-limited and no-match outcomes can still inflate or distort headline success numbers
- the DB golden harness is the better place to track semantic correctness
