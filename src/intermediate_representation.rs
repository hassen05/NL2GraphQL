use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Deserialize, Serialize)]
pub struct IRQuery {
    pub root_field: String,
    pub fields: Vec<String>,
    pub first: Option<i64>,
    pub offset: Option<i64>,
    pub filter: Option<Value>,
    pub order: Option<Value>,
}

pub(crate) fn graphql_value(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string()),
        Value::Array(arr) => {
            let inner = arr.iter().map(graphql_value).collect::<Vec<_>>().join(", ");
            format!("[{inner}]")
        }
        Value::Object(map) => {
            let inner = map
                .iter()
                .map(|(k, v)| format!("{k}: {}", graphql_value(v)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{{{inner}}}")
        }
    }
}

fn graphql_enum_or_string(value: &str) -> String {
    if Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*$")
        .ok()
        .is_some_and(|re| re.is_match(value))
    {
        value.to_string()
    } else {
        serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
    }
}

fn graphql_order_value(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let inner = map
                .iter()
                .map(|(k, v)| {
                    if (k == "asc" || k == "desc")
                        && let Value::String(s) = v
                    {
                        return format!("{k}: {}", graphql_enum_or_string(s));
                    }
                    format!("{k}: {}", graphql_value(v))
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("{{{inner}}}")
        }
        _ => graphql_value(value),
    }
}

#[derive(Default)]
struct FieldNode {
    children: std::collections::BTreeMap<String, FieldNode>,
    leaf: bool,
}

fn insert_field(root: &mut FieldNode, field: &str) {
    let parts: Vec<&str> = field.split('.').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return;
    }
    let mut node = root;
    for (i, part) in parts.iter().enumerate() {
        node = node.children.entry(part.to_string()).or_default();
        if i == parts.len() - 1 {
            node.leaf = true;
        }
    }
}

fn render_fields(node: &FieldNode, indent: usize) -> String {
    let mut out = String::new();
    let pad = " ".repeat(indent);
    for (name, child) in &node.children {
        if child.children.is_empty() {
            out.push_str(&format!("{pad}{name}\n"));
        } else {
            out.push_str(&format!("{pad}{name} {{\n"));
            out.push_str(&render_fields(child, indent + 2));
            out.push_str(&format!("{pad}}}\n"));
        }
    }
    out
}

pub fn ir_to_graphql(ir: &IRQuery) -> Option<String> {
    if ir.fields.is_empty() {
        return None;
    }

    let mut order = ir.order.clone();
    if let Some(Value::Object(map)) = &order {
        let mut new_map = map.clone();
        for key in ["asc", "desc"] {
            if let Some(Value::Array(arr)) = map.get(key) {
                if let Some(Value::String(s)) = arr.first() {
                    new_map.insert(key.to_string(), Value::String(s.clone()));
                } else {
                    new_map.remove(key);
                }
            }
        }
        order = Some(Value::Object(new_map));
    }

    let mut args = Vec::new();
    if let Some(first) = ir.first {
        args.push(format!("first: {first}"));
    }
    if let Some(offset) = ir.offset {
        args.push(format!("offset: {offset}"));
    }
    if let Some(filter) = &ir.filter {
        args.push(format!("filter: {}", graphql_value(filter)));
    }
    if let Some(order) = &order {
        args.push(format!("order: {}", graphql_order_value(order)));
    }

    let args_block = if args.is_empty() {
        String::new()
    } else {
        format!("({})", args.join(", "))
    };
    let mut root = FieldNode::default();
    for field in &ir.fields {
        insert_field(&mut root, field);
    }
    let fields_block = render_fields(&root, 4).trim_end().to_string();

    Some(format!(
        "query AutoIR {{\n  {}{} {{\n    {}\n  }}\n}}",
        ir.root_field, args_block, fields_block
    ))
}
