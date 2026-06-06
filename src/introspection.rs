use crate::schema_registry::SchemaRegistry;
use once_cell::sync::Lazy;
use regex::Regex;

static QUERY_NAME_FROM_PROMPT_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\bquery[A-Za-z0-9_]+\b").expect("valid query-name from prompt regex")
});

fn extract_query_name_from_prompt(user_message: &str) -> Option<String> {
    QUERY_NAME_FROM_PROMPT_RE
        .find(user_message)
        .map(|m| m.as_str().to_string())
}

pub(crate) fn introspection_answer(
    registry: &SchemaRegistry,
    user_message: &str,
) -> Option<String> {
    let msg = user_message.to_lowercase();
    if msg.contains("supported root query names")
        || msg.contains("list root query names")
        || msg.contains("show root query names")
    {
        let mut roots = registry
            .root_fields()
            .into_iter()
            .filter(|r| r.starts_with("query"))
            .collect::<Vec<_>>();
        roots.sort();
        return Some(format!(
            "Introspection:\nSupported root query names ({}):\n{}",
            roots.len(),
            roots.join(", ")
        ));
    }

    let query_name = extract_query_name_from_prompt(user_message)?;
    if msg.contains("valid filter fields") {
        if let Some(filter_input) = registry.query_filter_input(&query_name) {
            let mut fields = registry
                .input_field_names(filter_input)
                .map(|s| s.iter().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            fields.sort();
            return Some(format!(
                "Introspection:\nQuery: {query_name}\nFilter input: {filter_input}\nValid filter fields: {}",
                fields.join(", ")
            ));
        }
        return Some(format!(
            "Introspection:\nQuery: {query_name}\nNo filter input is defined for this query root."
        ));
    }

    if msg.contains("valid sortable fields") || msg.contains("valid order fields") {
        if let Some(order_input) = registry.query_order_input(&query_name) {
            let mut fields = registry
                .input_field_names(order_input)
                .map(|s| s.iter().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            fields.sort();
            return Some(format!(
                "Introspection:\nQuery: {query_name}\nOrder input: {order_input}\nValid sortable fields: {}",
                fields.join(", ")
            ));
        }
        return Some(format!(
            "Introspection:\nQuery: {query_name}\nNo order input is defined for this query root."
        ));
    }
    None
}
