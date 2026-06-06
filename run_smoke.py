#!/usr/bin/env python3
import argparse
import http.cookiejar
import json
import os
import re
import time
import urllib.error
import urllib.parse
import urllib.request
from collections import Counter
from pathlib import Path

IR_BLOCK_RE = re.compile(r"IR:\s*```json\s*(\{.*?\})\s*```", re.DOTALL)
QUERY_BLOCK_RE = re.compile(r"Query:\s*```graphql\s*(.*?)\s*```", re.DOTALL)
PLAN_STEP_RE = re.compile(r"^\s*\d+\.\s+.+$", re.MULTILINE)
MULTISTEP_QUERY_RE = re.compile(r"Query\s+\d+.*?```graphql\s*(.*?)\s*```", re.DOTALL | re.IGNORECASE)

ERROR_MARKERS = [
    "plan parsing:",
    "expected valid planv2 output after repair",
    "completionerror",
    "invalid status code",
    "ir execution error",
    "ir error:",
    "multi-step execution error",
    "multi-step query validation failed",
    "http status ",
    "graphql_validation_failed",
    "cannot query field",
    "is not defined by type",
    "expected value of type",
    "requires sub-selection",
    "invalid graphql syntax",
    "query parse error",
]

FINAL_ANSWER_RE = re.compile(r"Final Answer:\s*(.*?)(?:\n\n[A-Z][A-Za-z _-]+:|\Z)", re.DOTALL)


def endpoint_url(endpoint: str, path: str) -> str:
    parts = urllib.parse.urlsplit(endpoint)
    return urllib.parse.urlunsplit((parts.scheme, parts.netloc, path, "", ""))


def admin_opener(
    endpoint: str,
    username: str,
    password: str,
    timeout_sec: int,
) -> urllib.request.OpenerDirector:
    cookie_jar = http.cookiejar.CookieJar()
    opener = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(cookie_jar))
    payload = {"username": username, "password": password}
    req = urllib.request.Request(
        endpoint_url(endpoint, "/auth/login"),
        data=json.dumps(payload).encode("utf-8"),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with opener.open(req, timeout=timeout_sec) as resp:
        if resp.status != 200:
            raise RuntimeError(f"admin login failed with HTTP {resp.status}")
        resp.read()
    return opener


def post_chat(
    endpoint: str,
    model: str,
    prompt: str,
    debug: bool,
    timeout_sec: int,
    opener: urllib.request.OpenerDirector | None = None,
) -> dict:
    payload = {
        "model": model,
        "stream": False,
        "execute": True,
        "dry_run": debug,
        "messages": [{"role": "user", "content": prompt}],
    }
    req = urllib.request.Request(
        endpoint,
        data=json.dumps(payload).encode("utf-8"),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    client = opener or urllib.request
    with client.open(req, timeout=timeout_sec) as resp:
        return json.loads(resp.read().decode("utf-8"))


def extract_ir(content: str) -> dict | None:
    m = IR_BLOCK_RE.search(content or "")
    if not m:
        return None
    try:
        return json.loads(m.group(1))
    except json.JSONDecodeError:
        return None


def extract_query(content: str) -> str | None:
    m = QUERY_BLOCK_RE.search(content or "")
    if not m:
        return None
    return m.group(1).strip()


def extract_multistep_queries(content: str) -> list[str]:
    return [m.strip() for m in MULTISTEP_QUERY_RE.findall(content or "")]


def extract_final_answer(content: str) -> str | None:
    m = FINAL_ANSWER_RE.search(content or "")
    if not m:
        return None
    answer = m.group(1).strip()
    return answer or None


def extract_final_response(content: str) -> str | None:
    answer = extract_final_answer(content)
    if answer:
        return answer

    text = (content or "").strip()
    if not text:
        return None

    debug_markers = [
        "\n\nIR:",
        "\n\nRaw Planner JSON:",
        "\n\nRewrites:",
        "\n\nPlan:",
        "\n\nQuery 1",
        "\n\nEffective Executed Queries:",
        "\n\nDEBUG_PREP_LOGS:",
        "\n\nscope_used:",
        "\n\nDeterministic (pre-LLM):",
        "\n\ngrounding_confidence:",
        "\n\nProvenance:",
    ]
    cut = len(text)
    for marker in debug_markers:
        idx = text.find(marker)
        if idx != -1:
            cut = min(cut, idx)
    trimmed = text[:cut].strip()
    if not trimmed:
        return None
    return trimmed


def extract_named_json_block(content: str, label: str) -> dict | None:
    marker = f"{label}:\n```json"
    text = content or ""
    start = text.find(marker)
    if start == -1:
        return None
    json_start = start + len(marker)
    end = text.find("\n```", json_start)
    if end == -1:
        return None
    raw = text[json_start:end].strip()
    try:
        return json.loads(raw)
    except json.JSONDecodeError:
        return None


def extract_provenance(content: str) -> dict | None:
    return extract_named_json_block(content, "Provenance")


def extract_api_usage(payload: dict) -> dict | None:
    usage = payload.get("usage")
    if not isinstance(usage, dict):
        return None
    out = {}
    for key in ("prompt_tokens", "completion_tokens", "total_tokens"):
        value = usage.get(key)
        if isinstance(value, int):
            out[key] = value
    if not out or out.get("total_tokens", 0) <= 0:
        return None
    return out


def estimated_token_usage_from_provenance(content: str) -> dict | None:
    provenance = extract_provenance(content)
    if not isinstance(provenance, dict):
        return None
    metrics = provenance.get("metrics")
    if not isinstance(metrics, dict):
        return None
    input_tokens = 0
    output_tokens = 0
    for key in ("planner_prompt_tokens_est", "planner_repair_prompt_tokens_est"):
        value = metrics.get(key)
        if isinstance(value, int):
            input_tokens += value
    for key in (
        "planner_response_tokens_est",
        "planner_repair_response_tokens_est",
        "answer_tokens_est",
    ):
        value = metrics.get(key)
        if isinstance(value, int):
            output_tokens += value
    if input_tokens <= 0 and output_tokens <= 0:
        return None
    return {
        "input_tokens_est": input_tokens,
        "output_tokens_est": output_tokens,
        "total_tokens_est": input_tokens + output_tokens,
    }


def extract_provider_usage_from_provenance(content: str) -> dict | None:
    provenance = extract_provenance(content)
    if not isinstance(provenance, dict):
        return None
    metrics = provenance.get("metrics")
    if not isinstance(metrics, dict):
        return None
    usage = metrics.get("provider_token_usage")
    if not isinstance(usage, dict):
        return None
    out = {}
    for key in ("prompt_tokens", "completion_tokens", "total_tokens"):
        value = usage.get(key)
        if isinstance(value, int):
            out[key] = value
    if not out or out.get("total_tokens", 0) <= 0:
        return None
    return out


def extract_grounding_confidence(content: str) -> dict | None:
    direct = extract_named_json_block(content, "grounding_confidence")
    if direct is not None:
        return direct
    provenance = extract_provenance(content)
    if isinstance(provenance, dict):
        grounding = provenance.get("grounding_confidence")
        if isinstance(grounding, dict):
            return grounding
    return None


def grounding_stable_key_summary(grounding: dict | None) -> dict:
    if not isinstance(grounding, dict):
        return {
            "grounded_entity_key_count": 0,
            "stable_key_count": 0,
            "missing_stable_key_mentions": [],
        }

    grounded_keys = grounding.get("grounded_entity_keys")
    if not isinstance(grounded_keys, list):
        grounded_keys = []

    stable_key_count = 0
    for key in grounded_keys:
        if not isinstance(key, dict):
            continue
        if key.get("stable_key_field") and key.get("stable_key_value"):
            stable_key_count += 1

    missing = grounding.get("missing_stable_key_mentions")
    if not isinstance(missing, list):
        missing = []

    return {
        "grounded_entity_key_count": len(grounded_keys),
        "stable_key_count": stable_key_count,
        "missing_stable_key_mentions": [
            mention for mention in missing if isinstance(mention, str)
        ],
    }


def extract_schema_retrieval(content: str) -> dict | None:
    provenance = extract_provenance(content)
    if not isinstance(provenance, dict):
        return None
    retrieval = provenance.get("schema_retrieval")
    if isinstance(retrieval, dict):
        return retrieval
    return None


def schema_retrieval_root_names(retrieval: dict | None) -> list[str]:
    if not isinstance(retrieval, dict):
        return []
    roots = retrieval.get("roots")
    if not isinstance(roots, list):
        anchored = retrieval.get("anchored_roots")
        if isinstance(anchored, list):
            return [root for root in anchored if isinstance(root, str)]
        return []
    out = []
    for root in roots:
        if not isinstance(root, dict):
            continue
        name = root.get("root")
        if isinstance(name, str) and name:
            out.append(name)
    return out


def schema_retrieval_root_evidence(retrieval: dict | None, roots: list[str]) -> dict[str, list[str]]:
    if not isinstance(retrieval, dict):
        return {}
    wanted = {root for root in roots if isinstance(root, str) and root}
    if not wanted:
        return {}
    out: dict[str, list[str]] = {}
    for root in retrieval.get("roots") or []:
        if not isinstance(root, dict):
            continue
        name = root.get("root")
        if name not in wanted:
            continue
        evidence = root.get("capability_evidence")
        if isinstance(evidence, list):
            out[name] = [item for item in evidence if isinstance(item, str)]
        else:
            out[name] = []
    return out


def compact_json(value, limit: int = 260) -> str:
    if value in (None, "", [], {}):
        return ""
    text = json.dumps(value, ensure_ascii=False, sort_keys=True)
    if len(text) > limit:
        return text[: limit - 1] + "…"
    return text


def md_cell(value) -> str:
    text = str(value or "")
    return text.replace("|", "\\|").replace("\n", "<br>")


def schema_retrieval_root_rank(retrieved_roots: list[str], target_root: str | None) -> int | None:
    if not target_root:
        return None
    try:
        return retrieved_roots.index(target_root) + 1
    except ValueError:
        return None


def grounding_calibration_bucket(result: dict) -> str:
    if result.get("grounding_missing_stable_key_mentions"):
        return "stable_key_gap"

    overall = result.get("grounding_overall")
    if overall in ("clarification_needed", "needs_confirmation"):
        return str(overall)
    if result.get("semantic_issues"):
        return "semantic_mismatch"

    competitive_roots = result.get("schema_retrieval_competitive_roots")
    retrieval_confidence = result.get("schema_retrieval_confidence")
    executed_missing = result.get("schema_retrieval_missing_executed_roots") or []
    planned_missing = result.get("schema_retrieval_missing_planned_roots") or []
    selected_root_missing = result.get("schema_retrieval_selected_root_missing")
    executed_root_rank = result.get("schema_retrieval_executed_root_rank")
    planned_root_rank = result.get("schema_retrieval_planned_root_rank")
    selected_root_rank = result.get("schema_retrieval_selected_root_rank")
    top_matches_execution = result.get("schema_retrieval_top_root_matches_execution")

    if executed_missing:
        return "retrieval_missing_executed_root"
    if planned_missing:
        return "retrieval_missing_planned_root"
    if selected_root_missing is True:
        return "retrieval_missing_selected_root"
    if retrieval_confidence == "high":
        if isinstance(executed_root_rank, int) and executed_root_rank > 1:
            return "overconfident_retrieval_rank"
        if isinstance(planned_root_rank, int) and planned_root_rank > 1:
            return "overconfident_retrieval_rank"
        if isinstance(selected_root_rank, int) and selected_root_rank > 1:
            return "overconfident_retrieval_rank"
    if top_matches_execution is False and isinstance(executed_root_rank, int) and executed_root_rank > 1:
        return "retrieval_top_root_mismatch"
    if isinstance(competitive_roots, int) and competitive_roots >= 4:
        return "retrieval_too_broad"
    if retrieval_confidence == "low":
        return "weak_retrieval"

    if result.get("grounding_overall") in ("high_confidence", "none"):
        return "ok_for_grounding_calibration"
    return "needs_trace_review"


def extract_root_field_from_provenance(content: str) -> str | None:
    provenance = extract_provenance(content)
    if not isinstance(provenance, dict):
        return None
    plan_steps = provenance.get("plan_steps")
    if not isinstance(plan_steps, list):
        return None
    for step in plan_steps:
        if isinstance(step, dict):
            root_field = step.get("root_field")
            if isinstance(root_field, str) and root_field:
                return root_field
    return None


def is_multistep_plan(content: str) -> bool:
    text = content or ""
    if "Plan:" not in text:
        return False
    if not PLAN_STEP_RE.search(text):
        return False
    return len(extract_multistep_queries(text)) >= 2


def content_has_error(raw_content: str) -> bool:
    text = (raw_content or "").lower()
    final_answer = extract_final_answer(raw_content)
    if final_answer:
        text = final_answer.lower()
    return any(marker in text for marker in ERROR_MARKERS)


def load_prompts(path: Path) -> list[dict]:
    text = path.read_text(encoding="utf-8")
    if path.suffix == ".jsonl":
        rows = []
        for i, line in enumerate(text.splitlines(), start=1):
            if not line.strip():
                continue
            obj = json.loads(line)
            prompt = obj.get("prompt") or obj.get("question") or ""
            if not prompt:
                continue
            row_id = obj.get("id") or f"q_{i:03d}"
            rows.append({"id": row_id, "prompt": prompt})
        return rows

    rows = []
    for i, line in enumerate(text.splitlines(), start=1):
        s = line.strip()
        if not s:
            continue
        m = re.match(r"^(\d+)\.\s+(.*)$", s)
        if m:
            rows.append({"id": f"q_{int(m.group(1)):03d}", "prompt": m.group(2).strip()})
        else:
            rows.append({"id": f"q_{i:03d}", "prompt": s})
    return rows


def classify(raw_content: str, ir: dict | None, query: str | None) -> str:
    text = (raw_content or "").lower()
    if not text.strip():
        return "empty_answer"
    grounding = extract_grounding_confidence(raw_content)
    if isinstance(grounding, dict):
        overall = grounding.get("overall")
        if overall in ("clarification_needed", "needs_confirmation"):
            return str(overall)
    if "plan parsing:" in text or "expected valid planv2 output after repair" in text:
        return "planning_error"
    if "completionerror" in text or "invalid status code" in text:
        return "model_error"
    if extract_final_answer(raw_content):
        return "ok_multistep" if is_multistep_plan(raw_content) else "ok_answer"
    if "multi-step execution error" in text or "multi-step query validation failed" in text:
        return "multistep_execution_error"
    if "ir error:" in text:
        return "ir_error"
    if "ir execution error" in text:
        return "execution_http_error"
    if (
        "http status " in text
        or "graphql_validation_failed" in text
        or "cannot query field" in text
        or "is not defined by type" in text
        or "expected value of type" in text
        or "requires sub-selection" in text
        or "invalid graphql syntax" in text
        or "query parse error" in text
    ):
        return "execution_graphql_error"
    if "introspection:" in text:
        return "ok_introspection"
    if "found " in text or "computed distance" in text or "compared predicted vs actual power" in text:
        return "ok_answer"
    if is_multistep_plan(raw_content):
        return "ok_multistep"
    if ir is None:
        return "ok_answer"
    if query is None:
        return "query_missing"
    root = ir.get("root_field")
    fields = ir.get("fields") or []
    if not isinstance(root, str) or not root.startswith("query"):
        return "bad_root"
    if not isinstance(fields, list) or not fields:
        return "empty_fields"
    return "ok"


def semantic_audit(case: dict, result: dict) -> list[str]:
    if result.get("status") not in ("ok", "ok_multistep", "ok_introspection", "ok_answer"):
        return []
    prompt = case["prompt"].lower()
    root = result.get("root_field") or ""
    raw = result.get("raw_content") or ""
    issues: list[str] = []

    if ("scada" in prompt or "10-minute" in prompt or "active power" in prompt or "vibration" in prompt) and root not in ("queryHistoricalScadaAgg10min", "queryScadaSignal"):
        issues.append("scada_intent_root_mismatch")
    if "stop event" in prompt and "metric" not in prompt and root != "queryStopEvents":
        issues.append("stop_intent_root_mismatch")
    if (
        ("weather" in prompt or "access window" in prompt)
        and "tag" not in prompt
        and "category" not in prompt
        and root != "queryWeatherPrediction"
    ):
        issues.append("weather_intent_root_mismatch")
    if "power curve" in prompt and root != "queryPowerCurve":
        issues.append("power_curve_root_mismatch")
    if "vessel position" in prompt and root != "queryHistoricalAisVesselpos":
        issues.append("vessel_position_root_mismatch")
    if "vessel list" in prompt and root != "queryVessel":
        issues.append("vessel_list_root_mismatch")
    if ("by tag" in prompt or "tag for" in prompt) and root == "queryHistoricalScadaAgg10min":
        if "filter:" not in raw or "tag:" not in raw:
            issues.append("tag_filter_missing")
    return issues


def run_case(
    case: dict,
    endpoint: str,
    model: str,
    debug: bool,
    timeout_sec: int,
    opener: urllib.request.OpenerDirector | None = None,
) -> dict:
    started = time.time()
    out = {"id": case["id"], "prompt": case["prompt"]}
    try:
        payload = post_chat(endpoint, model, case["prompt"], debug, timeout_sec, opener)
        content = payload.get("choices", [{}])[0].get("message", {}).get("content", "")
        ir = extract_ir(content)
        query = extract_query(content)
        status = classify(content, ir, query)
        grounding = extract_grounding_confidence(content) or {}
        grounding_keys = grounding_stable_key_summary(grounding)
        schema_retrieval = extract_schema_retrieval(content) or {}
        schema_retrieval_roots = schema_retrieval_root_names(schema_retrieval)
        root_field = (ir or {}).get("root_field") or extract_root_field_from_provenance(content)
        selected_root_rank = schema_retrieval_root_rank(schema_retrieval_roots, root_field)
        top_root = schema_retrieval_roots[0] if schema_retrieval_roots else None
        evidence_roots = [root for root in (top_root, root_field) if root]
        root_evidence = schema_retrieval_root_evidence(schema_retrieval, evidence_roots)
        out.update(
            {
                "status": status,
                "root_field": root_field,
                "fields_count": len((ir or {}).get("fields") or []),
                "multistep_queries": len(extract_multistep_queries(content)),
                "contains_error_text": content_has_error(content),
                "grounding_overall": grounding.get("overall"),
                "grounding_clarification_recommended": grounding.get("clarification_recommended"),
                "grounding_grounded_entity_key_count": grounding_keys[
                    "grounded_entity_key_count"
                ],
                "grounding_stable_key_count": grounding_keys["stable_key_count"],
                "grounding_missing_stable_key_mentions": grounding_keys[
                    "missing_stable_key_mentions"
                ],
                "final_response": extract_final_response(content),
                "raw_content": content,
                "latency_ms": int((time.time() - started) * 1000),
                "api_usage": extract_api_usage(payload),
                "provider_usage": extract_provider_usage_from_provenance(content),
                "tokens_est": estimated_token_usage_from_provenance(content),
                "schema_retrieval_mode": schema_retrieval.get("mode"),
                "schema_retrieval_confidence": schema_retrieval.get("confidence"),
                "schema_retrieval_competitive_roots": schema_retrieval.get("competitive_root_count"),
                "schema_retrieval_top_roots": schema_retrieval_roots[:5],
                "schema_retrieval_top_root": top_root,
                "schema_retrieval_selected_root_rank": selected_root_rank,
                "schema_retrieval_selected_root_missing": bool(root_field and selected_root_rank is None),
                "schema_retrieval_top_root_evidence": root_evidence.get(top_root or "", []),
                "schema_retrieval_selected_root_evidence": root_evidence.get(root_field or "", []),
            }
        )
        out["semantic_issues"] = semantic_audit(case, out)
        out["grounding_calibration_bucket"] = grounding_calibration_bucket(out)
        return out
    except urllib.error.HTTPError as e:
        out.update({"status": "http_error", "error": f"HTTP {e.code}"})
    except Exception as e:
        out.update({"status": "runner_error", "error": str(e)})
    out["latency_ms"] = int((time.time() - started) * 1000)
    return out


def write_markdown(results: list[dict], out_md: Path) -> None:
    total = len(results)
    success_statuses = ("ok", "ok_answer", "ok_multistep", "ok_introspection")
    ok_single = sum(1 for r in results if r.get("status") in ("ok", "ok_answer"))
    ok_multistep = sum(1 for r in results if r.get("status") == "ok_multistep")
    ok_introspection = sum(1 for r in results if r.get("status") == "ok_introspection")
    ok = ok_single + ok_multistep + ok_introspection
    clean_ok = sum(
        1
        for r in results
        if r.get("status") in success_statuses and not r.get("contains_error_text", False)
    )
    avg_latency = int(sum(r.get("latency_ms", 0) for r in results) / max(total, 1))
    status_counts = Counter(r.get("status", "unknown") for r in results)
    root_counts = Counter(r.get("root_field") or "(none)" for r in results)
    grounding_counts = Counter(r.get("grounding_overall") or "(none)" for r in results)
    grounding_stable_key_total = sum(
        int(r.get("grounding_stable_key_count") or 0) for r in results
    )
    grounding_missing_stable_key_cases = [
        r for r in results if r.get("grounding_missing_stable_key_mentions")
    ]
    grounding_calibration_counts = Counter(
        r.get("grounding_calibration_bucket") or "(none)" for r in results
    )
    retrieval_mode_counts = Counter(r.get("schema_retrieval_mode") or "(none)" for r in results)
    retrieval_confidence_counts = Counter(
        r.get("schema_retrieval_confidence") or "(none)" for r in results
    )
    retrieval_root_counts = Counter(
        root
        for r in results
        for root in (r.get("schema_retrieval_top_roots") or [])
    )
    weak_retrieval_cases = [
        r
        for r in results
        if r.get("schema_retrieval_confidence") == "low"
        or (
            isinstance(r.get("schema_retrieval_competitive_roots"), int)
            and r.get("schema_retrieval_competitive_roots") >= 4
        )
    ]
    retrieval_rank_cases = [
        r
        for r in results
        if r.get("schema_retrieval_selected_root_missing")
        or (
            isinstance(r.get("schema_retrieval_selected_root_rank"), int)
            and r.get("schema_retrieval_selected_root_rank") > 1
        )
    ]
    semantic_issue_counts = Counter(
        issue
        for r in results
        for issue in (r.get("semantic_issues") or [])
    )
    semantic_flagged = sum(1 for r in results if (r.get("semantic_issues") or []))

    lines = [
        "# Smoke Eval Report (Unlabeled)",
        "",
        f"- Total prompts: {total}",
        f"- IR+Query success: {ok}",
        f"- Single-query success: {ok_single}",
        f"- Multi-step plan success: {ok_multistep}",
        f"- Introspection success: {ok_introspection}",
        f"- Success rate: {(ok / total * 100) if total else 0:.1f}%",
        f"- Clean success (no embedded error text): {clean_ok}",
        f"- Clean success rate: {(clean_ok / total * 100) if total else 0:.1f}%",
        f"- Avg latency: {avg_latency} ms",
        "",
        "## Status Breakdown",
        "",
        "| status | count |",
        "|---|---:|",
    ]
    for status, count in status_counts.most_common():
        lines.append(f"| {status} | {count} |")

    lines.extend([
        "",
        "## Root Distribution",
        "",
        "| root_field | count |",
        "|---|---:|",
    ])
    for root, count in root_counts.most_common(20):
        lines.append(f"| {root} | {count} |")

    lines.extend([
        "",
        "## Grounding / Clarification Breakdown",
        "",
        "| grounding_overall | count |",
        "|---|---:|",
    ])
    for overall, count in grounding_counts.most_common():
        lines.append(f"| {overall} | {count} |")

    lines.extend([
        "",
        f"- Grounded entity keys surfaced: {sum(int(r.get('grounding_grounded_entity_key_count') or 0) for r in results)}",
        f"- Grounded matches with stable keys: {grounding_stable_key_total}",
        f"- Cases missing stable keys: {len(grounding_missing_stable_key_cases)}",
        "",
        "| id | stable_keys | missing_stable_key_mentions |",
        "|---|---:|---|",
    ])
    for r in grounding_missing_stable_key_cases:
        missing = ", ".join(r.get("grounding_missing_stable_key_mentions") or [])
        lines.append(
            f"| {r.get('id')} | {int(r.get('grounding_stable_key_count') or 0)} | {missing} |"
        )

    lines.extend([
        "",
        "### Grounding Calibration Buckets",
        "",
        "| bucket | count |",
        "|---|---:|",
    ])
    for bucket, count in grounding_calibration_counts.most_common():
        lines.append(f"| {bucket} | {count} |")

    lines.extend([
        "",
        "| id | bucket | grounding | retrieval_confidence | selected_root_rank | competitive_roots | semantic_issues |",
        "|---|---|---|---|---:|---:|---|",
    ])
    for r in results:
        bucket = r.get("grounding_calibration_bucket")
        if bucket in (None, "ok_for_grounding_calibration"):
            continue
        issues = ", ".join(r.get("semantic_issues") or [])
        competitive = r.get("schema_retrieval_competitive_roots")
        selected_rank = r.get("schema_retrieval_selected_root_rank")
        lines.append(
            f"| {r.get('id')} | {bucket} | {r.get('grounding_overall') or ''} | {r.get('schema_retrieval_confidence') or ''} | {selected_rank if isinstance(selected_rank, int) else ''} | {competitive if isinstance(competitive, int) else ''} | {issues} |"
        )

    lines.extend([
        "",
        "## Schema Retrieval Breakdown",
        "",
        "| mode | count |",
        "|---|---:|",
    ])
    for mode, count in retrieval_mode_counts.most_common():
        lines.append(f"| {mode} | {count} |")

    lines.extend([
        "",
        "| confidence | count |",
        "|---|---:|",
    ])
    for confidence, count in retrieval_confidence_counts.most_common():
        lines.append(f"| {confidence} | {count} |")

    lines.extend([
        "",
        "| retrieved_root | top-5 appearances |",
        "|---|---:|",
    ])
    for root, count in retrieval_root_counts.most_common(20):
        lines.append(f"| {root} | {count} |")

    lines.extend([
        "",
        "### Selected Root Rank Misses",
        "",
        "| id | root_field | confidence | selected_root_rank | top_root | selected_root_evidence | top_root_evidence | top_roots |",
        "|---|---|---|---:|---|---|---|---|",
    ])
    for r in retrieval_rank_cases:
        roots = ", ".join(r.get("schema_retrieval_top_roots") or [])
        selected_rank = r.get("schema_retrieval_selected_root_rank")
        selected_evidence = compact_json(r.get("schema_retrieval_selected_root_evidence"))
        top_evidence = compact_json(r.get("schema_retrieval_top_root_evidence"))
        lines.append(
            f"| {md_cell(r.get('id'))} | {md_cell(r.get('root_field'))} | {md_cell(r.get('schema_retrieval_confidence'))} | {selected_rank if isinstance(selected_rank, int) else 'missing'} | {md_cell(r.get('schema_retrieval_top_root'))} | {md_cell(selected_evidence)} | {md_cell(top_evidence)} | {md_cell(roots)} |"
        )

    lines.extend([
        "",
        "### Weak / Broad Retrieval Cases",
        "",
        "| id | confidence | competitive_roots | top_roots |",
        "|---|---|---:|---|",
    ])
    for r in weak_retrieval_cases:
        roots = ", ".join(r.get("schema_retrieval_top_roots") or [])
        competitive = r.get("schema_retrieval_competitive_roots")
        top_evidence = compact_json(r.get("schema_retrieval_top_root_evidence"))
        lines.append(
            f"| {md_cell(r.get('id'))} | {md_cell(r.get('schema_retrieval_confidence'))} | {competitive if isinstance(competitive, int) else ''} | {md_cell(roots)} {md_cell(' evidence=' + top_evidence if top_evidence else '')} |"
        )

    lines.extend([
        "",
        "## Semantic Audit (Heuristic)",
        "",
        f"- Prompts flagged with likely intent mismatch: {semantic_flagged}",
        f"- Flagged rate: {(semantic_flagged / total * 100) if total else 0:.1f}%",
        "",
        "| issue_type | count |",
        "|---|---:|",
    ])
    for issue_type, count in semantic_issue_counts.most_common():
        lines.append(f"| {issue_type} | {count} |")

    lines.extend([
        "",
        "## Failed Cases",
        "",
        "| id | status | root_field | latency_ms |",
        "|---|---|---|---:|",
    ])
    for r in results:
        if r.get("status") not in success_statuses or r.get("contains_error_text", False):
            lines.append(
                f"| {r.get('id')} | {r.get('status')} | {r.get('root_field') or ''} | {r.get('latency_ms', 0)} |"
            )

    lines.extend([
        "",
        "## Questions and Final Responses",
        "",
    ])
    for r in results:
        response = (r.get("final_response") or "").strip() or "_No final response extracted._"
        lines.extend([
            f"### {r.get('id')}",
            "",
            f"**Question:** {r.get('prompt')}",
            "",
            f"**Response:** {response}",
            "",
        ])

    out_md.write_text("\n".join(lines) + "\n", encoding="utf-8")


def main() -> int:
    parser = argparse.ArgumentParser(description="Run unlabeled smoke eval on user questions.")
    parser.add_argument("--endpoint", default="http://localhost:8080/v1/chat/completions")
    parser.add_argument("--model", default="")
    parser.add_argument("--prompts", default="expected_user_questions.txt")
    parser.add_argument("--out-json", default="eval/results/smoke_300.json")
    parser.add_argument("--out-md", default="eval/results/smoke_300.md")
    parser.add_argument("--debug", action="store_true", help="Enable dry_run to include Plan/Query in responses")
    parser.add_argument("--timeout-sec", type=int, default=180, help="HTTP timeout per prompt")
    parser.add_argument(
        "--admin-username",
        default=os.environ.get("EVAL_ADMIN_USERNAME") or os.environ.get("ADMIN_USERNAME") or "admin",
        help="Admin username for debug smoke auth. Defaults to EVAL_ADMIN_USERNAME, ADMIN_USERNAME, or admin.",
    )
    parser.add_argument(
        "--admin-password",
        default=os.environ.get("EVAL_ADMIN_PASSWORD") or os.environ.get("ADMIN_PASSWORD") or "",
        help="Admin password for debug smoke auth. Defaults to EVAL_ADMIN_PASSWORD or ADMIN_PASSWORD.",
    )
    args = parser.parse_args()

    cases = load_prompts(Path(args.prompts))
    results = []
    total = len(cases)
    ok_so_far = 0
    opener = None

    if args.debug:
        if args.admin_password:
            try:
                opener = admin_opener(
                    args.endpoint,
                    args.admin_username,
                    args.admin_password,
                    args.timeout_sec,
                )
                print(f"Admin debug session established for `{args.admin_username}`.")
            except Exception as exc:
                print(f"Warning: admin debug login failed: {exc}")
        else:
            print(
                "Warning: --debug requires admin auth after the admin-only debug change. "
                "Pass --admin-password, or set EVAL_ADMIN_PASSWORD/ADMIN_PASSWORD."
            )

    for idx, case in enumerate(cases, start=1):
        result = run_case(case, args.endpoint, args.model, args.debug, args.timeout_sec, opener)
        results.append(result)
        if result.get("status") in ("ok", "ok_answer", "ok_multistep", "ok_introspection"):
            ok_so_far += 1
        pct = (idx / total * 100) if total else 100.0
        print(
            f"\rProgress: {idx}/{total} ({pct:.1f}%) | OK: {ok_so_far} | Fail: {idx-ok_so_far} | {case['id']}",
            end="",
            flush=True,
        )
    print()

    out_json = Path(args.out_json)
    out_md = Path(args.out_md)
    out_json.parent.mkdir(parents=True, exist_ok=True)
    out_md.parent.mkdir(parents=True, exist_ok=True)
    out_json.write_text(json.dumps(results, indent=2), encoding="utf-8")
    write_markdown(results, out_md)

    ok = sum(1 for r in results if r.get("status") in ("ok", "ok_answer", "ok_multistep", "ok_introspection"))
    print(f"Total: {len(results)}  OK: {ok}  Failed: {len(results)-ok}")
    print(f"JSON: {out_json}")
    print(f"MD:   {out_md}")
    return 0 if ok == len(results) else 1


if __name__ == "__main__":
    raise SystemExit(main())
