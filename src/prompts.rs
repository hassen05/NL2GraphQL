pub(crate) struct PlannerPromptContext<'a> {
    pub(crate) today_utc: &'a str,
    pub(crate) root_fields: &'a str,
    pub(crate) preferred_root_fields: &'a str,
    pub(crate) planner_examples: &'a str,
    pub(crate) entity_mention_hints: &'a str,
    pub(crate) entity_resolution_hints: &'a str,
    pub(crate) policy_hints: &'a str,
    pub(crate) join_hints: &'a str,
    pub(crate) metric_hints: &'a str,
    pub(crate) field_hints: &'a str,
    pub(crate) schema_snippet: &'a str,
    pub(crate) user_message: &'a str,
}

pub(crate) fn build_planner_prompt(ctx: &PlannerPromptContext<'_>) -> String {
    format!(
        "Output ONLY one JSON object that matches the PlanV2 schema. No prose.\n\
         Required top-level keys:\n\
         - version: \"v2\"\n\
         - rewrites: [string]\n\
         - notes: [string]\n\
         - steps: [PlanV2Step]\n\
         Each step must be one of:\n\
         1) fetch:\n\
            {{\"id\":\"s1\",\"op\":\"fetch\",\"root_field\":\"queryX\",\"fields\":[\"a\",\"b\"],\"filter\":null,\"order\":null,\"first\":null,\"offset\":null}}\n\
         2) aggregate:\n\
            {{\"id\":\"s2\",\"op\":\"aggregate\",\"source\":\"s1\",\"group_by\":[\"a\"],\"metrics\":[{{\"op\":\"count\"}},{{\"op\":\"avg\",\"field\":\"value\"}},{{\"op\":\"metric\",\"name\":\"downtime_hours\"}}]}}\n\
         3) compare:\n\
            {{\"id\":\"s3\",\"op\":\"compare\",\"left\":\"s1\",\"right\":\"s2\",\"metric\":{{\"op\":\"avg\",\"field\":\"timestamp\"}}}}\n\
         4) filter_rows:\n\
            {{\"id\":\"s4\",\"op\":\"filter_rows\",\"source\":\"s1\",\"field\":\"shortName\",\"operator\":\"contains\",\"value\":\"A\"}}\n\
         5) rank:\n\
            {{\"id\":\"s5\",\"op\":\"rank\",\"source\":\"s2\",\"by\":\"count\",\"direction\":\"desc\",\"limit\":10}}\n\
         6) distance_haversine:\n\
            {{\"id\":\"s6\",\"op\":\"distance_haversine\",\"vessels_source\":\"s1\",\"target_source\":\"s2\"}}\n\
         7) join_on_time:\n\
            {{\"id\":\"s7\",\"op\":\"join_on_time\",\"left\":\"s1\",\"right\":\"s2\",\"left_time_field\":\"timestamp\",\"right_time_field\":\"time\",\"window_minutes\":10}}\n\
         8) threshold_check:\n\
            {{\"id\":\"s8\",\"op\":\"threshold_check\",\"source\":\"s3\",\"field\":\"distanceKm\",\"operator\":\"<=\",\"value\":1.0}}\n\
         9) trend_summary:\n\
            {{\"id\":\"s9\",\"op\":\"trend_summary\",\"source\":\"s1\",\"time_field\":\"time\",\"value_field\":\"windSpeed10m\"}}\n\
         Rules:\n\
         - Include ALL user constraints (time/entity/location/id) in fetch filters.\n\
         - Resolve relative dates/times using current UTC date `{today_utc}`.\n\
         - Prefer exact schema field names. If SLS canonical field defaults are provided for a generic term, use those exact fields instead of inventing aliases.\n\
         - Use only schema-valid root fields and field paths.\n\
         - Every fetch field path must end on a scalar leaf field. Do not put object/relation fields like `location` or `historicalAisVesselpos` by themselves in `fields`; instead select exact nested leaf paths such as `location.point.latitude`.\n\
         - When a later fetch depends on an earlier fetch result, bind the scalar with a placeholder in the filter value, e.g. `{{\"mmsi\":{{\"eq\":\"${{s1.mmsi}}\"}}}}`.\n\
         - For filter arguments, use only operator keys defined in the filter input types shown in the schema; do not invent operators.\n\
         - When a filter input field expects a list (e.g., `[String!]`), pass a list value.\n\
         - For order arguments, follow the order input shape from the schema and set exactly one direction key (asc or desc).\n\
         - Root field in fetch must be one of: {root_fields}\n\
         - Prefer fetch root_field from these likely roots first: {preferred_root_fields}\n\
         - Only choose a different root when these likely roots cannot satisfy all user constraints.\n\
         - Choose the operator chain explicitly from user intent:\n\
           * single entity lookup/details -> fetch with scoped filter\n\
           * list with exact scope -> fetch with scoped filter\n\
           * contains-style post-filtering -> fetch then filter_rows\n\
           * how many / total count -> fetch then aggregate count\n\
           * count by / group by X -> fetch X then aggregate with group_by=[X] and count\n\
           * average / sum / min / max -> fetch needed field(s) then aggregate\n\
           * top/highest/most/largest/max -> use rank with direction desc\n\
           * bottom/lowest/least/smallest/min -> use rank with direction asc\n\
           * compare two scoped groups -> build left fetch + aggregate, right fetch + aggregate, then compare\n\
           * trend / over time / increasing / decreasing -> fetch time + value fields, order by time when supported, then trend_summary\n\
           * vessel/entity distance queries that need identity plus positions -> fetch the entity id first, then fetch its position rows from the position root; do not treat a nested relation as a flat scalar field\n\
         - Prefer explicit typed steps over vague broad fetches: use fetch filters for exact scoped constraints, use filter_rows for post-fetch contains-style filtering, use aggregate for counts/averages/sums, use rank for top/bottom requests, and use compare only after computing the left/right datasets.\n\
         - For plain single-entity detail lookups, keep fetch fields focused on the entity's own scalar detail fields. Do not include child relation paths like `hasOffshoreWindTurbine.*` or `hasOffshoreSubstation.*` unless the user explicitly asks for related/member entities.\n\
         - For parent-child membership requests (for example wind farm -> turbines), prefer an explicit parent relation path such as `hasOffshoreWindTurbine.<leaf>` when the schema/SLS indicates it, instead of guessing by binding a parent display field like `plantId` into a child UID filter.\n\
         - For compare/count-by/top-N questions, do not answer with a single broad fetch when a typed operator chain is needed.\n\
         - Never output free-form GraphQL query strings.\n\
         - Never output template placeholders like `${{s1.value}}`.\n\
         Patterns to imitate:\n\
         {planner_examples}\n\
         {entity_mention_hints}\n\
         {entity_resolution_hints}\n\
         {policy_hints}\
         {join_hints}\
         {metric_hints}\
         {field_hints}\
         Use this planner context:\n\
         {schema_snippet}\n\
        User request: {user_message}\n",
        today_utc = ctx.today_utc,
        root_fields = ctx.root_fields,
        preferred_root_fields = ctx.preferred_root_fields,
        planner_examples = ctx.planner_examples,
        entity_mention_hints = ctx.entity_mention_hints,
        entity_resolution_hints = ctx.entity_resolution_hints,
        policy_hints = ctx.policy_hints,
        join_hints = ctx.join_hints,
        metric_hints = ctx.metric_hints,
        field_hints = ctx.field_hints,
        schema_snippet = ctx.schema_snippet,
        user_message = ctx.user_message,
    )
}

pub(crate) struct PlanRepairPromptContext<'a> {
    pub(crate) root_fields: &'a str,
    pub(crate) preferred_root_fields: &'a str,
    pub(crate) today_utc: &'a str,
    pub(crate) entity_mention_hints: &'a str,
    pub(crate) entity_resolution_hints: &'a str,
    pub(crate) schema_snippet: &'a str,
    pub(crate) policy_hints: &'a str,
    pub(crate) join_hints: &'a str,
    pub(crate) metric_hints: &'a str,
    pub(crate) field_hints: &'a str,
    pub(crate) previous_error: &'a str,
    pub(crate) input: &'a str,
}

pub(crate) fn build_plan_repair_prompt(ctx: &PlanRepairPromptContext<'_>) -> String {
    format!(
        "Repair the following model output into valid PlanV2 JSON only.\n\
         Output ONLY one JSON object matching PlanV2; no prose.\n\
         Use version=\"v2\".\n\
         Keep user intent and constraints.\n\
         Fetch root_field must be one of: {root_fields}\n\
         Prefer fetch root_field from these likely roots first: {preferred_root_fields}\n\
         Only choose a different root when these likely roots cannot satisfy all user constraints.\n\
         Resolve relative dates/times using current UTC date `{today_utc}`.\n\
         Prefer exact schema field names. If SLS canonical field defaults are provided for a generic term, use those exact fields instead of inventing aliases.\n\
         Every fetch field path must end on a scalar leaf field. Do not keep object/relation fields like `location` or `historicalAisVesselpos` by themselves in `fields`; repair them into exact nested leaf paths or separate fetch steps.\n\
         When a later fetch depends on an earlier fetch result, preserve or repair scalar placeholder bindings like `{{\"mmsi\":{{\"eq\":\"${{s1.mmsi}}\"}}}}` instead of dropping the dependency.\n\
         If a fetch filter is not schema-valid but the user still requires that constraint, preserve it by fetching the field and adding an equivalent `filter_rows` step with the same field/operator/value. Never drop a threshold or equality constraint without an equivalent post-fetch replacement.\n\
         If the request is a plain single-entity detail lookup, remove unrelated child relation fields unless the user explicitly asked for related/member entities.\n\
         {entity_mention_hints}\n\
         {entity_resolution_hints}\n\
         Use this planner context:\n\
         {schema_snippet}\n\
         {policy_hints}\
         {join_hints}\
         {metric_hints}\
         {field_hints}\
         Previous validation/policy error: {previous_error}\n\
         Input:\n\
         {input}\n",
        root_fields = ctx.root_fields,
        preferred_root_fields = ctx.preferred_root_fields,
        today_utc = ctx.today_utc,
        entity_mention_hints = ctx.entity_mention_hints,
        entity_resolution_hints = ctx.entity_resolution_hints,
        schema_snippet = ctx.schema_snippet,
        policy_hints = ctx.policy_hints,
        join_hints = ctx.join_hints,
        metric_hints = ctx.metric_hints,
        field_hints = ctx.field_hints,
        previous_error = ctx.previous_error,
        input = ctx.input,
    )
}

pub(crate) struct AnswerSynthesisPromptContext<'a> {
    pub(crate) user_message: &'a str,
    pub(crate) evidence_text: &'a str,
    pub(crate) fallback_answer: &'a str,
}

pub(crate) fn build_answer_synthesis_prompt(ctx: &AnswerSynthesisPromptContext<'_>) -> String {
    format!(
        "User question:\n{user_message}\n\n\
         Evidence (JSON):\n{evidence_text}\n\n\
         Fallback deterministic answer:\n{fallback_answer}\n\n\
         Task:\n\
         - Write a clear final answer for a non-technical user.\n\
         - Use only facts in Evidence.\n\
         - If data is insufficient, state that clearly.\n\
         - Keep it concise (1-3 sentences).\n\
         - Do NOT invent units; if evidence has no unit, keep values unit-neutral.\n\
         - Preserve scope/time exactly as evidenced.",
        user_message = ctx.user_message,
        evidence_text = ctx.evidence_text,
        fallback_answer = ctx.fallback_answer,
    )
}

pub(crate) struct QueryRepairPromptContext<'a> {
    pub(crate) forbidden_fields_text: &'a str,
    pub(crate) today_utc: &'a str,
    pub(crate) step_scope: &'a str,
    pub(crate) step_constraints_text: &'a str,
    pub(crate) user_message: &'a str,
    pub(crate) error_text: &'a str,
    pub(crate) root_fields: &'a str,
    pub(crate) dataset_ctx: &'a str,
    pub(crate) repair_ctx: &'a str,
    pub(crate) schema_snippet: &'a str,
    pub(crate) broken_query: &'a str,
}

pub(crate) fn build_query_repair_prompt(ctx: &QueryRepairPromptContext<'_>) -> String {
    format!(
        "Fix the following GraphQL query so it is valid for the provided schema.\n\
         Output ONLY the corrected GraphQL query (no markdown, no prose).\n\
         Keep user intent and scope.\n\
         STRICT RULES:\n\
         - Return a full executable GraphQL document.\n\
         - Do NOT use template placeholders like ${{s1.field}}.\n\
         - Use only operator names that exist in the schema for each filter type.\n\
         - For filter fields whose input type is an object, pass an object value with valid operator keys; never pass raw scalars.\n\
         - When a filter input field expects a list (e.g., `[String!]`), pass a list value.\n\
         - For order arguments, follow the order input shape and set exactly one direction key (asc or desc).\n\
         - Keep nested object selections valid; object fields must use proper sub-selection paths defined by schema.\n\
         - If backend error includes a Query-level \"Did you mean ...\" suggestion, prefer that suggested root field.\n\
         - If backend validation says a field is missing/invalid, remove that field even if schema snippets look permissive.\n\
         - Fields to avoid from last backend error: {forbidden_fields_text}\n\
         - If user uses relative time (today, yesterday, this week, last week, this month), resolve using current UTC date `{today_utc}`.\n\
         - If the previous query returned 0 rows, do NOT broaden plain-name/entity scope or guess alternate roots.\n\
         - For 0-row repairs, only apply schema/shape fixes or, when the broken query already contains the same compact identifier-style literal (for example OSS-003, WF4, T115), rewrite across schema-known identifier fields while preserving the same root and literal.\n\
         - Do NOT relax exact identifier-style equality constraints (for values like OSS-003, WF4, PLANT-005) from `eq` to `contains` unless the exact-match form is preserved.\n\
         - If a referenced prior-step value is unavailable, remove that filter instead of inventing placeholders.\n\
         - Keep this step scoped only to literals already present in the broken query unless the backend error explicitly requires a schema-level field/operator rewrite.\n\
         - Do NOT import sibling compare-branch entity values from the user request into this step if they are not already present in the broken query.\n\
         - {step_scope}\n\
         Step-local constraints extracted from the broken query: {step_constraints_text}\n\
         User request: {user_message}\n\
         Validation/execution error: {error_text}\n\
         Allowed root fields: {root_fields}\n\
         Available prior-step datasets: {dataset_ctx}\n\
         Additional introspection hints:\n{repair_ctx}\n\
         Relevant schema:\n{schema_snippet}\n\
         Broken query:\n{broken_query}\n",
        forbidden_fields_text = ctx.forbidden_fields_text,
        today_utc = ctx.today_utc,
        step_scope = ctx.step_scope,
        step_constraints_text = ctx.step_constraints_text,
        user_message = ctx.user_message,
        error_text = ctx.error_text,
        root_fields = ctx.root_fields,
        dataset_ctx = ctx.dataset_ctx,
        repair_ctx = ctx.repair_ctx,
        schema_snippet = ctx.schema_snippet,
        broken_query = ctx.broken_query,
    )
}

#[cfg(test)]
mod tests {
    use super::{
        PlanRepairPromptContext, PlannerPromptContext, build_plan_repair_prompt,
        build_planner_prompt,
    };

    #[test]
    fn planner_prompt_surfaces_preferred_root_fields() {
        let prompt = build_planner_prompt(&PlannerPromptContext {
            today_utc: "2026-04-21",
            root_fields: "queryOffshoreWindFarm, queryOffshoreWindTurbine",
            preferred_root_fields: "queryOffshoreWindFarm, queryOffshoreWindTurbine",
            planner_examples: "",
            entity_mention_hints: "",
            entity_resolution_hints: "",
            policy_hints: "",
            join_hints: "",
            metric_hints: "",
            field_hints: "",
            schema_snippet: "Likely query roots and usable fields:",
            user_message: "How many turbines does each wind farm have?",
        });

        assert!(
            prompt.contains(
                "Prefer fetch root_field from these likely roots first: queryOffshoreWindFarm, queryOffshoreWindTurbine"
            ),
            "expected preferred-root guidance in planner prompt: {prompt}"
        );
    }

    #[test]
    fn repair_prompt_surfaces_preferred_root_fields() {
        let prompt = build_plan_repair_prompt(&PlanRepairPromptContext {
            root_fields: "queryOffshoreWindFarm, queryOffshoreWindTurbine",
            preferred_root_fields: "queryOffshoreWindFarm",
            today_utc: "2026-04-21",
            entity_mention_hints: "",
            entity_resolution_hints: "",
            schema_snippet: "Likely query roots and usable fields:",
            policy_hints: "",
            join_hints: "",
            metric_hints: "",
            field_hints: "",
            previous_error: "invalid field",
            input: "{}",
        });

        assert!(
            prompt.contains(
                "Prefer fetch root_field from these likely roots first: queryOffshoreWindFarm"
            ),
            "expected preferred-root guidance in repair prompt: {prompt}"
        );
    }

    #[test]
    fn planner_prompt_warns_against_child_relations_for_plain_detail_lookups() {
        let prompt = build_planner_prompt(&PlannerPromptContext {
            today_utc: "2026-04-21",
            root_fields: "queryOffshoreWindFarm",
            preferred_root_fields: "queryOffshoreWindFarm",
            planner_examples: "",
            entity_mention_hints: "",
            entity_resolution_hints: "",
            policy_hints: "",
            join_hints: "",
            metric_hints: "",
            field_hints: "",
            schema_snippet: "Likely query roots and usable fields:",
            user_message: "Show details for wind farm shortName \"WF3\".",
        });

        assert!(
            prompt.contains(
                "Do not include child relation paths like `hasOffshoreWindTurbine.*` or `hasOffshoreSubstation.*` unless the user explicitly asks for related/member entities."
            ),
            "expected plain-detail child relation guard in planner prompt: {prompt}"
        );
    }

    #[test]
    fn repair_prompt_warns_against_child_relations_for_plain_detail_lookups() {
        let prompt = build_plan_repair_prompt(&PlanRepairPromptContext {
            root_fields: "queryOffshoreWindFarm",
            preferred_root_fields: "queryOffshoreWindFarm",
            today_utc: "2026-04-21",
            entity_mention_hints: "",
            entity_resolution_hints: "",
            schema_snippet: "Likely query roots and usable fields:",
            policy_hints: "",
            join_hints: "",
            metric_hints: "",
            field_hints: "",
            previous_error: "invalid child relation fields on plain detail lookup",
            input: "{}",
        });

        assert!(
            prompt.contains(
                "If the request is a plain single-entity detail lookup, remove unrelated child relation fields unless the user explicitly asked for related/member entities."
            ),
            "expected plain-detail child relation guard in repair prompt: {prompt}"
        );
    }
}
