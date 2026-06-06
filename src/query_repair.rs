#![allow(
    clippy::needless_raw_string_hashes,
    clippy::non_std_lazy_statics,
    clippy::redundant_pub_crate,
    clippy::suspicious_operation_groupings
)]

use crate::schema_registry::SchemaRegistry;
use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::HashSet;

static PLACEHOLDER_PATTERN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\$\{[^}]+\}").expect("valid placeholder pattern"));
static ROOT_FIELD_PATTERN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\b(query[A-Za-z0-9_]+)\s*(?:\(|\{)").expect("valid root pattern"));
static BACKEND_INVALID_FIELD_RE_1: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"Cannot query field\s+"([A-Za-z_][A-Za-z0-9_]*)""#)
        .expect("valid backend-invalid-field pattern 1")
});
static BACKEND_INVALID_FIELD_RE_2: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"Cannot query field\s+\\\"([A-Za-z_][A-Za-z0-9_]*)\\\""#)
        .expect("valid backend-invalid-field pattern 2")
});
static BACKEND_INVALID_FIELD_RE_3: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"Field\s+"([A-Za-z_][A-Za-z0-9_]*)"\s+is not defined by type"#)
        .expect("valid backend-invalid-field pattern 3")
});
static BACKEND_INVALID_FIELD_RE_4: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"Field\s+\\\"([A-Za-z_][A-Za-z0-9_]*)\\\"\s+is not defined by type"#)
        .expect("valid backend-invalid-field pattern 4")
});
static BACKEND_INVALID_FIELD_RE_5: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"Unknown field\s+"([A-Za-z_][A-Za-z0-9_]*)"\s+on type"#)
        .expect("valid backend-invalid-field pattern 5")
});
static BACKEND_INVALID_FIELD_RE_6: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"Unknown field\s+\\\"([A-Za-z_][A-Za-z0-9_]*)\\\"\s+on type"#)
        .expect("valid backend-invalid-field pattern 6")
});
static ORDER_ERROR_PATTERN_1: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"Expected value of type\s+"([A-Za-z0-9_]+Order)",\s+found\s+(asc|desc)"#)
        .expect("valid order error pattern 1")
});
static ORDER_ERROR_PATTERN_2: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"Expected value of type\s+\\\"([A-Za-z0-9_]+Order)\\\",\s+found\s+(asc|desc)"#)
        .expect("valid order error pattern 2")
});
static ORDER_ERROR_PATTERN_3: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r#"Expected object for input type ['"]([A-Za-z0-9_]+Order)['"] at ['"]order['"], got ['"]?(asc|desc)['"]?"#,
    )
    .expect("valid order error pattern 3")
});
static ORDER_BY_WITH_BARE_ORDER_PATTERN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r#"(?is)\borderBy\s*:\s*([A-Za-z_][A-Za-z0-9_]*)\s*,?\s*order\s*:\s*"?((?:asc|desc))"?"#,
    )
    .expect("valid orderBy + bare order pattern")
});
static FILTER_ALIAS_GTE_PATTERN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\bgte\s*:").expect("valid gte alias pattern"));
static FILTER_ALIAS_LTE_PATTERN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\blte\s*:").expect("valid lte alias pattern"));
static NUMERIC_EQ_FOR_STRING_PATTERN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"([A-Za-z_][A-Za-z0-9_]*)\s*:\s*\{\s*eq\s*:\s*([0-9]+)\s*\}"#)
        .expect("valid numeric eq for string pattern")
});
static UNKNOWN_ROOT_SUGGESTION_PATTERN_1: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"Cannot query field\s+"([A-Za-z_][A-Za-z0-9_]*)"\s+on type\s+"Query"\.\s+Did you mean\s+"([A-Za-z_][A-Za-z0-9_]*)"\?"#)
        .expect("valid unknown-root suggestion pattern 1")
});
static UNKNOWN_ROOT_SUGGESTION_PATTERN_2: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"Cannot query field\s+\\\"([A-Za-z_][A-Za-z0-9_]*)\\\"\s+on type\s+\\\"Query\\\"\.\s+Did you mean\s+\\\"([A-Za-z_][A-Za-z0-9_]*)\\\"\?"#)
        .expect("valid unknown-root suggestion pattern 2")
});
static UNKNOWN_ROOT_SUGGESTION_PATTERN_3: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"Cannot query field\s+'([A-Za-z_][A-Za-z0-9_]*)'\s+on type\s+'Query'\.\s+Did you mean\s+'([A-Za-z_][A-Za-z0-9_]*)'\?"#)
        .expect("valid unknown-root suggestion pattern 3")
});
static UNKNOWN_ROOT_SUGGESTION_PATTERN_4: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"Cannot query field\s+\\'([A-Za-z_][A-Za-z0-9_]*)\\'\s+on type\s+\\'Query\\'\.\s+Did you mean\s+\\'([A-Za-z_][A-Za-z0-9_]*)\\'\?"#)
        .expect("valid unknown-root suggestion pattern 4")
});
static ERROR_LINE_PATTERN_1: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#""line"\s*:\s*([0-9]+)"#).expect("valid error line pattern 1"));
static ERROR_LINE_PATTERN_2: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"\\\"line\\\"\s*:\s*([0-9]+)"#).expect("valid error line pattern 2"));
static INVALID_FILTER_TYPE_ERROR_RE_1: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r#"Field\s+"([A-Za-z_][A-Za-z0-9_]*)"\s+is not defined by type\s+"[A-Za-z0-9_]*Filter""#,
    )
    .expect("valid invalid-filter-type pattern 1")
});
static INVALID_FILTER_TYPE_ERROR_RE_2: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"Field\s+\\\"([A-Za-z_][A-Za-z0-9_]*)\\\"\s+is not defined by type\s+\\\"[A-Za-z0-9_]*Filter\\\""#)
        .expect("valid invalid-filter-type pattern 2")
});
static FIRST_ONE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"\bfirst\s*:\s*1\b"#).expect("valid first=1 regex"));
static IDENTIFIER_EQ_LITERAL_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"eq\s*:\s*"([^"]+)""#).expect("valid eq literal regex"));
static EMPTY_SELECTION_SET_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?s)\b[A-Za-z_][A-Za-z0-9_]*\s*(?:\([^{}]*\))?\s*\{\s*\}"#)
        .expect("valid empty selection set regex")
});

fn extract_root_field_from_query(query: &str) -> Option<String> {
    ROOT_FIELD_PATTERN
        .captures_iter(query)
        .find_map(|caps| caps.get(1).map(|m| m.as_str().to_string()))
}

fn has_empty_selection_set(query: &str) -> bool {
    EMPTY_SELECTION_SET_RE.is_match(query)
}

pub(crate) fn has_unresolved_placeholders(query: &str) -> bool {
    PLACEHOLDER_PATTERN.is_match(query)
}

pub(crate) fn extract_backend_invalid_fields(error_text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let patterns = [
        &*BACKEND_INVALID_FIELD_RE_1,
        &*BACKEND_INVALID_FIELD_RE_2,
        &*BACKEND_INVALID_FIELD_RE_3,
        &*BACKEND_INVALID_FIELD_RE_4,
        &*BACKEND_INVALID_FIELD_RE_5,
        &*BACKEND_INVALID_FIELD_RE_6,
    ];
    for re in patterns {
        for caps in re.captures_iter(error_text) {
            if let Some(m) = caps.get(1) {
                let field = m.as_str().to_string();
                if !out.iter().any(|v: &String| v == &field) {
                    out.push(field);
                    if out.len() >= 20 {
                        return out;
                    }
                }
            }
        }
    }
    out
}

fn looks_like_identifier_literal(v: &str) -> bool {
    let s = v.trim();
    if s.is_empty() {
        return false;
    }
    let compacted = s
        .chars()
        .filter(|ch| !ch.is_ascii_whitespace())
        .collect::<String>();
    if compacted.is_empty() {
        return false;
    }
    let has_digit = compacted.chars().any(|c| c.is_ascii_digit());
    let has_sep = compacted.contains('-') || compacted.contains('_');
    let upper_short = compacted.len() <= 20
        && s.chars().all(|c| {
            c.is_ascii_uppercase()
                || c.is_ascii_digit()
                || c == '-'
                || c == '_'
                || c.is_ascii_whitespace()
        });
    if s.contains(char::is_whitespace) {
        return upper_short && (has_digit || has_sep);
    }
    has_digit || has_sep || upper_short
}

pub(crate) fn maybe_expand_identifier_eq_filter(
    schema_registry: &SchemaRegistry,
    expected_root: Option<&str>,
    query: &str,
) -> Option<String> {
    fn expand_identifier_variants(v: &str) -> Vec<String> {
        fn push_unique(out: &mut Vec<String>, value: String) {
            if !out.iter().any(|existing| existing == &value) {
                out.push(value);
            }
        }

        let trimmed = v.trim();
        if trimmed.is_empty() {
            return Vec::new();
        }
        let mut out = vec![trimmed.to_string()];
        let Ok(re) = Regex::new(r"^(?P<prefix>.*?)(?P<num>\d+)$") else {
            return out;
        };
        if let Some(caps) = re.captures(trimmed) {
            let prefix = caps.name("prefix").map(|m| m.as_str()).unwrap_or("");
            let num = caps.name("num").map(|m| m.as_str()).unwrap_or("");
            if !prefix.is_empty() && !num.is_empty() {
                let prefix_trimmed = prefix.trim_end();
                if prefix_trimmed.len() != prefix.len() {
                    let compact = format!("{prefix_trimmed}{num}");
                    push_unique(&mut out, compact);
                }
                let normalized_num = num
                    .parse::<u64>()
                    .ok()
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| num.to_string());
                let mut numeric_forms = vec![num.to_string()];
                push_unique(&mut numeric_forms, normalized_num);

                let mut prefixes = vec![prefix.to_string()];
                if prefix_trimmed.len() != prefix.len() {
                    push_unique(&mut prefixes, prefix_trimmed.to_string());
                    push_unique(&mut prefixes, format!("{prefix_trimmed} "));
                    push_unique(&mut prefixes, format!("{prefix_trimmed}  "));
                }

                for prefix_variant in prefixes {
                    for numeric in &numeric_forms {
                        push_unique(&mut out, format!("{prefix_variant}{numeric}"));
                        if numeric.len() < 3 {
                            for width in (numeric.len() + 1)..=3 {
                                push_unique(&mut out, format!("{prefix_variant}{numeric:>width$}"));
                            }
                            push_unique(&mut out, format!("{prefix_variant}{numeric:0>3}"));
                        }
                    }
                }
            }
        }
        out
    }

    let root_name = expected_root
        .map(|s| s.to_string())
        .or_else(|| extract_root_field_from_query(query))?;
    let filter_input = schema_registry.query_filter_input(&root_name)?;
    let filter_fields = schema_registry.input_field_names(filter_input)?;
    if !filter_fields.contains("or") {
        return None;
    }
    let id_fields = schema_registry.root_identifier_filter_fields(&root_name);
    if id_fields.len() < 2 {
        return None;
    }

    let inline_eq_re =
        Regex::new(r#"([A-Za-z_][A-Za-z0-9_]*)\s*:\s*\{\s*eq\s*:\s*"([^"]+)"\s*\}"#).ok()?;
    let mut rewritten_inline = query.to_string();
    let mut inline_replacements = Vec::new();
    for caps in inline_eq_re.captures_iter(query) {
        let Some(full) = caps.get(0) else {
            continue;
        };
        let Some(field_match) = caps.get(1) else {
            continue;
        };
        let Some(value_match) = caps.get(2) else {
            continue;
        };
        let field_name = field_match.as_str();
        if !id_fields.iter().any(|field| field == field_name) {
            continue;
        }
        let value = value_match.as_str();
        if !looks_like_identifier_literal(value) {
            continue;
        }
        let mut variants = expand_identifier_variants(value);
        let mut seen = HashSet::new();
        variants.retain(|variant| seen.insert(variant.clone()));
        if variants.len() < 2 {
            continue;
        }
        let values = variants
            .iter()
            .filter_map(|variant| serde_json::to_string(variant).ok())
            .collect::<Vec<_>>()
            .join(", ");
        inline_replacements.push((
            full.start(),
            full.end(),
            format!("{field_name}: {{in: [{values}]}}"),
        ));
    }
    if !inline_replacements.is_empty() {
        for (start, end, replacement) in inline_replacements.into_iter().rev() {
            rewritten_inline.replace_range(start..end, &replacement);
        }
        if rewritten_inline != query {
            return Some(rewritten_inline);
        }
    }

    let eq_obj_re = Regex::new(
        r#"filter\s*:\s*\{\s*([A-Za-z_][A-Za-z0-9_]*)\s*:\s*\{\s*eq\s*:\s*"([^"]+)"\s*\}\s*\}"#,
    )
    .ok()?;
    let scalar_re =
        Regex::new(r#"filter\s*:\s*\{\s*([A-Za-z_][A-Za-z0-9_]*)\s*:\s*"([^"]+)"\s*\}"#).ok()?;
    let (field_name, eq_val) = if let Some(caps) = eq_obj_re.captures(query) {
        (
            caps.get(1)?.as_str().to_string(),
            caps.get(2)?.as_str().to_string(),
        )
    } else if let Some(caps) = scalar_re.captures(query) {
        (
            caps.get(1)?.as_str().to_string(),
            caps.get(2)?.as_str().to_string(),
        )
    } else {
        return None;
    };
    if !looks_like_identifier_literal(&eq_val) {
        return None;
    }
    let source_is_known_identifier_field = id_fields.iter().any(|f| f == &field_name);
    let mut eq_values = expand_identifier_variants(&eq_val);
    if eq_values.is_empty() {
        eq_values.push(eq_val.clone());
    }
    let mut seen_vals = HashSet::new();
    eq_values.retain(|v| seen_vals.insert(v.clone()));
    if !source_is_known_identifier_field {
        return None;
    }
    None
}

pub(crate) fn preserve_identifier_eq_semantics(
    original_query: &str,
    repaired_query: &str,
) -> Option<String> {
    let protected_literals = IDENTIFIER_EQ_LITERAL_RE
        .captures_iter(original_query)
        .filter_map(|caps| caps.get(1).map(|m| m.as_str().to_string()))
        .filter(|value| looks_like_identifier_literal(value))
        .collect::<Vec<_>>();

    if protected_literals.is_empty() {
        return None;
    }

    let mut rewritten = repaired_query.to_string();
    for literal in &protected_literals {
        let pattern = format!(r#"contains\s*:\s*"{}""#, regex::escape(literal));
        let Ok(re) = Regex::new(&pattern) else {
            continue;
        };
        rewritten = re
            .replace_all(&rewritten, format!(r#"eq: "{}""#, literal).as_str())
            .to_string();
    }

    if rewritten == repaired_query {
        None
    } else {
        Some(rewritten)
    }
}

fn maybe_rewrite_bare_order_arg(
    schema_registry: &SchemaRegistry,
    query: &str,
    error_text: &str,
) -> Option<String> {
    let mut order_input = None::<String>;
    let mut direction = None::<String>;
    for re in [
        &*ORDER_ERROR_PATTERN_1,
        &*ORDER_ERROR_PATTERN_2,
        &*ORDER_ERROR_PATTERN_3,
    ] {
        if let Some(caps) = re.captures(error_text) {
            order_input = caps.get(1).map(|m| m.as_str().to_string());
            direction = caps.get(2).map(|m| m.as_str().to_lowercase());
            break;
        }
    }
    let order_input = order_input?;
    let direction = direction?;
    if direction != "asc" && direction != "desc" {
        return None;
    }

    if let Some(caps) = ORDER_BY_WITH_BARE_ORDER_PATTERN.captures(query) {
        let order_by_field = caps.get(1)?.as_str();
        let rewritten = ORDER_BY_WITH_BARE_ORDER_PATTERN
            .replace(
                query,
                format!("order: {{{direction}: {order_by_field}}}").as_str(),
            )
            .to_string();
        if rewritten != query {
            return Some(rewritten);
        }
    }

    let _ = schema_registry.input_field_type(&order_input, &direction)?;
    None
}

pub(crate) fn maybe_rewrite_filter_operator_aliases(query: &str) -> Option<String> {
    let rewritten = FILTER_ALIAS_LTE_PATTERN
        .replace_all(&FILTER_ALIAS_GTE_PATTERN.replace_all(query, "ge:"), "le:")
        .to_string();
    if rewritten == query {
        None
    } else {
        Some(rewritten)
    }
}

fn maybe_rewrite_scalar_hash_filter(
    schema_registry: &SchemaRegistry,
    query: &str,
    error_text: &str,
) -> Option<String> {
    let lower = error_text.to_lowercase();
    if !lower.contains("stringhashfilter") && !lower.contains("timestampfilter") {
        return None;
    }
    let root_name = extract_root_field_from_query(query)?;
    let scalar_fields = schema_registry.root_filter_object_fields(&root_name);
    if scalar_fields.is_empty() {
        return None;
    }
    let key_union = scalar_fields
        .iter()
        .map(|f| regex::escape(f))
        .collect::<Vec<_>>()
        .join("|");
    let field_re = Regex::new(&format!(r#"({key_union})\s*:\s*"([^"]+)""#)).ok()?;
    let rewritten = field_re
        .replace_all(query, |caps: &regex::Captures<'_>| {
            format!(r#"{}: {{eq: "{}"}}"#, &caps[1], &caps[2])
        })
        .to_string();
    if rewritten == query {
        None
    } else {
        Some(rewritten)
    }
}

fn maybe_quote_numeric_eq_for_string(query: &str, error_text: &str) -> Option<String> {
    let lower = error_text.to_lowercase();
    if !(lower.contains("string cannot represent a non string value")
        || lower.contains("expected type string"))
    {
        return None;
    }
    let rewritten = NUMERIC_EQ_FOR_STRING_PATTERN
        .replace_all(query, r#"$1: {eq: "$2"}"#)
        .to_string();
    if rewritten == query {
        None
    } else {
        Some(rewritten)
    }
}

fn maybe_rewrite_unknown_root_with_suggestion(query: &str, error_text: &str) -> Option<String> {
    let mut old_root = None::<String>;
    let mut new_root = None::<String>;
    for re in [
        &*UNKNOWN_ROOT_SUGGESTION_PATTERN_1,
        &*UNKNOWN_ROOT_SUGGESTION_PATTERN_2,
        &*UNKNOWN_ROOT_SUGGESTION_PATTERN_3,
        &*UNKNOWN_ROOT_SUGGESTION_PATTERN_4,
    ] {
        if let Some(caps) = re.captures(error_text) {
            old_root = caps.get(1).map(|m| m.as_str().to_string());
            new_root = caps.get(2).map(|m| m.as_str().to_string());
            break;
        }
    }
    let old_root = old_root?;
    let new_root = new_root?;
    if old_root == new_root {
        return None;
    }

    let root_re = regex::Regex::new(&format!(r"\b{}\b(?=\s*\()", regex::escape(&old_root))).ok()?;
    let rewritten = root_re.replace_all(query, new_root.as_str()).to_string();
    if rewritten == query {
        None
    } else {
        Some(rewritten)
    }
}

fn extract_error_line_numbers(error_text: &str) -> Vec<usize> {
    let mut lines = Vec::new();
    for re in [&*ERROR_LINE_PATTERN_1, &*ERROR_LINE_PATTERN_2] {
        for caps in re.captures_iter(error_text) {
            if let Some(m) = caps.get(1)
                && let Ok(n) = m.as_str().parse::<usize>()
                && n > 0
                && !lines.contains(&n)
            {
                lines.push(n);
                if lines.len() >= 24 {
                    return lines;
                }
            }
        }
    }
    lines.sort_unstable();
    lines
}

fn maybe_prune_query_lines_from_error_locations(candidate: &str, last_err: &str) -> Option<String> {
    let err_lc = last_err.to_lowercase();
    let is_invalid_field_error = err_lc.contains("cannot query field")
        || err_lc.contains("is not defined by type")
        || err_lc.contains("unknown field");
    if !is_invalid_field_error {
        return None;
    }
    let line_numbers = extract_error_line_numbers(last_err);
    if line_numbers.is_empty() {
        return None;
    }
    let mut lines = candidate.lines().map(|s| s.to_string()).collect::<Vec<_>>();
    if lines.is_empty() {
        return None;
    }

    fn brace_delta(s: &str) -> i32 {
        s.chars().fold(0, |acc, ch| match ch {
            '{' => acc + 1,
            '}' => acc - 1,
            _ => acc,
        })
    }

    for line_no in line_numbers.into_iter().rev() {
        let idx = line_no.saturating_sub(1);
        if idx >= lines.len() {
            continue;
        }
        let trimmed = lines[idx].trim().to_string();
        if trimmed.is_empty() {
            continue;
        }
        // Guard against deleting the root field invocation line (e.g., inline invalid filter field).
        if ROOT_FIELD_PATTERN.is_match(&trimmed) && trimmed.contains('(') {
            continue;
        }

        let starts_block = trimmed.ends_with('{');
        let mut end_idx = idx;
        if starts_block {
            let mut depth = brace_delta(&lines[idx]);
            while depth > 0 && end_idx + 1 < lines.len() {
                end_idx += 1;
                depth += brace_delta(&lines[end_idx]);
            }
        }
        lines.drain(idx..=end_idx);
    }

    let mut rewritten = lines.join("\n");
    if let Ok(re) = Regex::new(r#"\{\s*,"#) {
        rewritten = re.replace_all(&rewritten, "{").to_string();
    }
    if let Ok(re) = Regex::new(r#",\s*\}"#) {
        rewritten = re.replace_all(&rewritten, "}").to_string();
    }
    if let Ok(re) = Regex::new(r#"\n{3,}"#) {
        rewritten = re.replace_all(&rewritten, "\n\n").to_string();
    }
    if rewritten.trim().is_empty() || rewritten == candidate {
        None
    } else {
        Some(rewritten)
    }
}

fn maybe_remove_invalid_selected_fields(candidate: &str, last_err: &str) -> Option<String> {
    let invalid_fields = extract_backend_invalid_fields(last_err);
    if invalid_fields.is_empty() {
        return None;
    }

    fn brace_delta(s: &str) -> i32 {
        s.chars().fold(0, |acc, ch| match ch {
            '{' => acc + 1,
            '}' => acc - 1,
            _ => acc,
        })
    }

    let mut lines = candidate
        .lines()
        .map(|line| line.to_string())
        .collect::<Vec<_>>();
    if lines.is_empty() {
        return None;
    }

    for idx in (0..lines.len()).rev() {
        let trimmed = lines[idx].trim();
        if trimmed.is_empty() || ROOT_FIELD_PATTERN.is_match(trimmed) {
            continue;
        }
        let field_name = trimmed
            .trim_end_matches('{')
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_end_matches('{')
            .trim();
        if !invalid_fields.iter().any(|field| field == field_name) {
            continue;
        }

        let mut end_idx = idx;
        if trimmed.ends_with('{') {
            let mut depth = brace_delta(&lines[idx]);
            while depth > 0 && end_idx + 1 < lines.len() {
                end_idx += 1;
                depth += brace_delta(&lines[end_idx]);
            }
        }
        lines.drain(idx..=end_idx);
    }

    let mut rewritten = lines.join("\n");
    if let Ok(re) = Regex::new(r#"\n{3,}"#) {
        rewritten = re.replace_all(&rewritten, "\n\n").to_string();
    }
    if rewritten.trim().is_empty() || rewritten == candidate || has_empty_selection_set(&rewritten)
    {
        None
    } else {
        Some(rewritten)
    }
}

fn maybe_rewrite_remove_invalid_filter_field_clauses(
    candidate: &str,
    last_err: &str,
) -> Option<String> {
    let mut invalid_fields = Vec::<String>::new();
    for re in [
        &*INVALID_FILTER_TYPE_ERROR_RE_1,
        &*INVALID_FILTER_TYPE_ERROR_RE_2,
    ] {
        for caps in re.captures_iter(last_err) {
            if let Some(m) = caps.get(1) {
                let field = m.as_str().to_string();
                if !invalid_fields.iter().any(|f| f == &field) {
                    invalid_fields.push(field);
                }
            }
        }
    }
    if invalid_fields.is_empty() {
        return None;
    }

    let mut rewritten = candidate.to_string();
    for field in invalid_fields {
        let esc = regex::escape(&field);
        let patterns = [
            format!(r#"\{{\s*{esc}\s*:\s*\{{[^{{}}]*\}}\s*\}}\s*,?"#), // list clause: {field:{...}}
            format!(r#",\s*{esc}\s*:\s*\{{[^{{}}]*\}}"#), // object clause with leading comma
            format!(r#"{esc}\s*:\s*\{{[^{{}}]*\}}\s*,?"#), // object clause direct
        ];
        for pat in patterns {
            if let Ok(re) = Regex::new(&pat) {
                rewritten = re.replace_all(&rewritten, "").to_string();
            }
        }
    }

    if let Ok(re) = Regex::new(r#"\[\s*,"#) {
        rewritten = re.replace_all(&rewritten, "[").to_string();
    }
    if let Ok(re) = Regex::new(r#",\s*\]"#) {
        rewritten = re.replace_all(&rewritten, "]").to_string();
    }
    if let Ok(re) = Regex::new(r#",\s*,+"#) {
        rewritten = re.replace_all(&rewritten, ",").to_string();
    }
    if let Ok(re) = Regex::new(r#"\{\s*,"#) {
        rewritten = re.replace_all(&rewritten, "{").to_string();
    }
    if let Ok(re) = Regex::new(r#",\s*\}"#) {
        rewritten = re.replace_all(&rewritten, "}").to_string();
    }

    if rewritten == candidate || rewritten.trim().is_empty() || has_empty_selection_set(&rewritten)
    {
        None
    } else {
        Some(rewritten)
    }
}

pub(crate) fn maybe_build_error_retry(
    schema_registry: &SchemaRegistry,
    candidate: &str,
    last_err: &str,
    attempt: usize,
    max_repair_attempts: usize,
) -> Option<(String, String)> {
    if attempt >= max_repair_attempts {
        return None;
    }
    if let Some(adapted) = maybe_rewrite_remove_invalid_filter_field_clauses(candidate, last_err) {
        return Some((
            " | Applied deterministic invalid-filter-field clause removal and retrying."
                .to_string(),
            adapted,
        ));
    }
    if let Some(adapted) = maybe_remove_invalid_selected_fields(candidate, last_err) {
        return Some((
            " | Applied deterministic invalid selected-field pruning and retrying.".to_string(),
            adapted,
        ));
    }
    if let Some(adapted) = maybe_prune_query_lines_from_error_locations(candidate, last_err) {
        return Some((
            " | Applied deterministic invalid-field pruning from backend error locations and retrying."
                .to_string(),
            adapted,
        ));
    }
    if let Some(adapted) = maybe_rewrite_unknown_root_with_suggestion(candidate, last_err) {
        return Some((
            " | Applied deterministic query-root suggestion rewrite and retrying.".to_string(),
            adapted,
        ));
    }
    if let Some(adapted) = maybe_rewrite_bare_order_arg(schema_registry, candidate, last_err) {
        return Some((
            " | Applied deterministic order-argument rewrite and retrying.".to_string(),
            adapted,
        ));
    }
    if let Some(adapted) = maybe_rewrite_filter_operator_aliases(candidate) {
        return Some((
            " | Applied deterministic filter-operator alias rewrite and retrying.".to_string(),
            adapted,
        ));
    }
    if let Some(adapted) = maybe_rewrite_scalar_hash_filter(schema_registry, candidate, last_err) {
        return Some((
            " | Applied deterministic scalar-to-filter rewrite and retrying.".to_string(),
            adapted,
        ));
    }
    if let Some(adapted) = maybe_quote_numeric_eq_for_string(candidate, last_err) {
        return Some((
            " | Applied deterministic numeric-to-string eq rewrite and retrying.".to_string(),
            adapted,
        ));
    }
    None
}

pub(crate) fn maybe_build_empty_rows_retry(
    schema_registry: &SchemaRegistry,
    candidate: &str,
    attempt: usize,
    max_repair_attempts: usize,
    root_name: &str,
    rows: &[serde_json::Value],
) -> Option<(String, Option<String>)> {
    fn relax_first_for_identifier_retry(query: &str) -> String {
        FIRST_ONE_RE.replace_all(query, "first: 200").to_string()
    }

    if !rows.is_empty() || !candidate.contains("filter:") || attempt >= max_repair_attempts {
        return None;
    }
    if let Some(adapted) =
        maybe_expand_identifier_eq_filter(schema_registry, Some(root_name), candidate)
    {
        let adapted = relax_first_for_identifier_retry(&adapted);
        let msg = format!(
            "Execution returned 0 rows for `{root_name}` with current filter. \
Applied identifier fallback across schema-discovered candidate fields and retrying."
        );
        return Some((msg, Some(adapted)));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifier_variants_expand_compact_uid() {
        let registry = SchemaRegistry::new(include_str!("../schemas/consumer_schema.graphql"));
        let query = r#"query {
            queryOffshoreWindTurbine(filter: { locationId: { eq: "FARM-UID-3" } }) {
                name
            }
        }"#;
        let rewritten =
            maybe_expand_identifier_eq_filter(&registry, Some("queryOffshoreWindTurbine"), query)
                .expect("should rewrite");
        assert!(
            rewritten.contains("FARM-UID- 3")
                && rewritten.contains("FARM-UID-  3")
                && rewritten.contains("FARM-UID-003"),
            "expected padded identifier variants in rewritten filter"
        );
    }

    #[test]
    fn identifier_variants_expand_zero_padded_shortname_to_space_padded_forms() {
        let registry = SchemaRegistry::new(include_str!("../schemas/consumer_schema.graphql"));
        let query = r#"query {
            queryOffshoreSubstation(filter: { shortName: { eq: "OSS-003" } }) {
                name
                shortName
            }
        }"#;
        let rewritten =
            maybe_expand_identifier_eq_filter(&registry, Some("queryOffshoreSubstation"), query)
                .expect("should rewrite");
        assert!(
            rewritten.contains("OSS-3")
                && rewritten.contains("OSS- 3")
                && rewritten.contains("OSS-  3")
                && rewritten.contains("OSS-003"),
            "expected compact, single-space, double-space, and zero-padded identifier variants"
        );
    }

    #[test]
    fn bare_order_arg_rewrite_uses_order_by_field() {
        let registry = SchemaRegistry::new(include_str!("../schemas/consumer_schema.graphql"));
        let query = r#"query ListOnshoreSubstationsForWindFarm5 {
  queryOnshoreSubstation(
    first: 200
    orderBy: name
    order: asc
  ) {
    id
    name
  }
}"#;
        let error =
            r#"Expected object for input type 'OnshoreSubstationOrder' at 'order', got "asc""#;
        let rewritten =
            maybe_rewrite_bare_order_arg(&registry, query, error).expect("should rewrite");
        assert!(rewritten.contains("order: {asc: name}"));
        assert!(!rewritten.contains("orderBy: name"));
        assert!(!rewritten.contains("order: asc"));
    }

    #[test]
    fn bare_order_arg_repair_does_not_guess_order_field() {
        let registry = SchemaRegistry::new(include_str!("../schemas/consumer_schema.graphql"));
        let query = r#"query ListOnshoreSubstationsForWindFarm5 {
  queryOnshoreSubstation(
    first: 200
    order: asc
  ) {
    id
    name
  }
}"#;
        let error =
            r#"Expected object for input type 'OnshoreSubstationOrder' at 'order', got "asc""#;

        assert!(
            maybe_rewrite_bare_order_arg(&registry, query, error).is_none(),
            "repair should not invent an order field from time/id/name heuristics"
        );
    }

    #[test]
    fn preserve_identifier_eq_semantics_upgrades_contains_back_to_eq() {
        let original = r#"query {
  queryOffshoreSubstation(filter: {shortName: {eq: "OSS-003"}}) { name }
}"#;
        let repaired = r#"query {
  queryOffshoreSubstation(filter: {or: [{shortName: {contains: "OSS-003"}}, {name: {contains: "OSS-003"}}]}) { name }
}"#;
        let rewritten =
            preserve_identifier_eq_semantics(original, repaired).expect("should rewrite");
        assert!(rewritten.contains(r#"shortName: {eq: "OSS-003"}"#));
        assert!(rewritten.contains(r#"name: {eq: "OSS-003"}"#));
    }

    #[test]
    fn identifier_retry_skips_plain_name_like_values() {
        let registry = SchemaRegistry::new(include_str!("../schemas/consumer_schema.graphql"));
        let query = r#"query {
            queryOffshoreWindTurbine(filter: { name: { eq: "turbine 115" } }) {
                accumulatedDowntime
                name
            }
        }"#;
        assert!(
            maybe_expand_identifier_eq_filter(&registry, Some("queryOffshoreWindTurbine"), query)
                .is_none(),
            "plain labels should not trigger hardcoded numbered-entity expansion"
        );
    }

    #[test]
    fn empty_rows_retry_skips_plain_name_like_values() {
        let registry = SchemaRegistry::new(include_str!("../schemas/consumer_schema.graphql"));
        let query = r#"query {
            queryOffshoreWindTurbine(first: 1, filter: { name: { eq: "turbine 115" } }) {
                accumulatedDowntime
                name
            }
        }"#;
        let rows: Vec<serde_json::Value> = Vec::new();
        assert!(
            maybe_build_empty_rows_retry(
                &registry,
                query,
                0,
                4,
                "queryOffshoreWindTurbine",
                &rows,
            )
            .is_none(),
            "empty-row retries should not reopen semantic planning for plain-name labels"
        );
    }

    #[test]
    fn identifier_retry_expands_compact_ids_inside_compound_filters() {
        let registry = SchemaRegistry::new(include_str!("../schemas/consumer_schema.graphql"));
        let query = r#"query {
            queryTag(first: 2000, filter: {categoryDescription: {eq: "Weather"}, plantId: {eq: "PLANT-5"}}) {
                id
            }
        }"#;
        let rewritten = maybe_expand_identifier_eq_filter(&registry, Some("queryTag"), query)
            .expect("compound identifier filters should preserve siblings and expand variants");
        assert!(rewritten.contains(r#"categoryDescription: {eq: "Weather"}"#));
        assert!(
            rewritten
                .contains(r#"plantId: {in: ["PLANT-5", "PLANT- 5", "PLANT-  5", "PLANT-005"]}"#)
        );
    }

    #[test]
    fn identifier_retry_expands_spaced_ids_on_same_field() {
        let registry = SchemaRegistry::new(include_str!("../schemas/consumer_schema.graphql"));
        let query = r#"query {
            queryTag(first: 2000, filter: {plantId: {eq: "PLANT- 4"}}) {
                status
            }
        }"#;
        let rewritten = maybe_expand_identifier_eq_filter(&registry, Some("queryTag"), query)
            .expect("spaced identifier filters should retry equivalent same-field forms");
        for expected in ["PLANT- 4", "PLANT-4", "PLANT-  4", "PLANT-004"] {
            assert!(
                rewritten.contains(expected),
                "expected {expected} in rewritten query: {rewritten}"
            );
        }
    }

    #[test]
    fn invalid_selected_field_pruning_does_not_emit_empty_selection_set() {
        let query = r#"query {
  queryOffshoreWindTurbine(first: 2000) {
    locationId
  }
}"#;
        let error = r#"GraphQL execution errors: Unknown field "locationId" on type "OffshoreWindTurbine". Did you mean "sapLocationId"?"#;

        assert!(
            maybe_remove_invalid_selected_fields(query, error).is_none(),
            "expected pruning to refuse emitting an empty selection set"
        );
    }
}
