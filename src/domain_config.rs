use crate::sls::{FieldRoles, Sls};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug, Default)]
pub struct FieldPosition {
    pub latitude_fields: Vec<String>,
    pub longitude_fields: Vec<String>,
    pub geo_object_fields: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct FieldRoleSet {
    pub id_fields: Vec<String>,
    pub entity_key_fields: Vec<String>,
    pub label_fields: Vec<String>,
    pub numeric_fields: Vec<String>,
    pub time_fields: Vec<String>,
    pub latitude_fields: Vec<String>,
    pub longitude_fields: Vec<String>,
    pub geo_object_fields: Vec<String>,
}

impl FieldRoleSet {
    pub fn merge(&self, other: &FieldRoleSet) -> FieldRoleSet {
        fn merge_vec(a: &[String], b: &[String]) -> Vec<String> {
            let mut out = a.to_vec();
            for v in b {
                if !out.iter().any(|e| e == v) {
                    out.push(v.clone());
                }
            }
            out
        }
        FieldRoleSet {
            id_fields: merge_vec(&self.id_fields, &other.id_fields),
            entity_key_fields: merge_vec(&self.entity_key_fields, &other.entity_key_fields),
            label_fields: merge_vec(&self.label_fields, &other.label_fields),
            numeric_fields: merge_vec(&self.numeric_fields, &other.numeric_fields),
            time_fields: merge_vec(&self.time_fields, &other.time_fields),
            latitude_fields: merge_vec(&self.latitude_fields, &other.latitude_fields),
            longitude_fields: merge_vec(&self.longitude_fields, &other.longitude_fields),
            geo_object_fields: merge_vec(&self.geo_object_fields, &other.geo_object_fields),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct DomainConfig {
    // stable lookup fields inferred from direct root arguments, scalar ID types, and SLS
    pub id_fields: Vec<String>,
    // broader lookup/display key fields inferred from root/filter exposure and SLS
    pub entity_key_fields: Vec<String>,
    // display fields inferred from filterable/direct string scalars and SLS
    pub label_fields: Vec<String>,
    // scalar numeric fields inferred from schema types (Int/Float and numeric-like scalars)
    pub numeric_fields: Vec<String>,
    // time-like fields inferred from schema types, filter operator structure, and SLS
    pub time_fields: Vec<String>,
    // GIS-capable fields inferred from coordinate-bearing schema structure and SLS
    pub location_fields: FieldPosition,
    // object type -> field-role set
    pub type_field_roles: HashMap<String, FieldRoleSet>,
    // root field -> field-role set
    pub root_field_roles: HashMap<String, FieldRoleSet>,
    // enum name -> values
    pub enum_values: HashMap<String, Vec<String>>,
    // root field -> filter fields aligned with inferred identifier/key/label roles
    pub root_identifier_filter_fields: HashMap<String, Vec<String>>,
    // root field -> filter fields expecting object operators (`*Filter` inputs)
    pub root_filter_object_fields: HashMap<String, Vec<String>>,
    // root field -> fields in `filter` input that are time/date-like according to schema types
    pub root_time_filter_fields: HashMap<String, Vec<String>>,
}

fn is_numeric_type_name(type_name: &str) -> bool {
    match type_name {
        "Int" | "Float" => true,
        _ => {
            let lower = type_name.to_lowercase();
            lower.contains("int")
                || lower.contains("float")
                || lower.contains("double")
                || lower.contains("decimal")
                || lower.contains("number")
        }
    }
}

fn is_string_like_type_name(type_name: &str) -> bool {
    match type_name {
        "String" | "ID" => true,
        _ => type_name.to_lowercase().contains("string"),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CoordinateAxis {
    Latitude,
    Longitude,
}

fn coordinate_axis(field_name: &str) -> Option<CoordinateAxis> {
    match field_name.to_ascii_lowercase().as_str() {
        "lat" | "latitude" => Some(CoordinateAxis::Latitude),
        "lon" | "lng" | "longitude" => Some(CoordinateAxis::Longitude),
        _ => None,
    }
}

fn paired_numeric_coordinate_fields(
    fields: &HashMap<String, String>,
) -> (HashSet<String>, HashSet<String>) {
    let mut latitude_fields = HashSet::new();
    let mut longitude_fields = HashSet::new();

    for (field_name, field_type) in fields {
        if !is_numeric_type_name(field_type) {
            continue;
        }
        match coordinate_axis(field_name) {
            Some(CoordinateAxis::Latitude) => {
                latitude_fields.insert(field_name.clone());
            }
            Some(CoordinateAxis::Longitude) => {
                longitude_fields.insert(field_name.clone());
            }
            None => {}
        }
    }

    if latitude_fields.is_empty() || longitude_fields.is_empty() {
        return (HashSet::new(), HashSet::new());
    }

    (latitude_fields, longitude_fields)
}

fn is_temporal_scalar_type(type_name: &str) -> bool {
    matches!(type_name, "Date" | "DateTime")
}

fn is_temporal_filter_type(type_name: &str) -> bool {
    matches!(
        type_name,
        "DateFilterInput" | "DateTimeFilter" | "TimeStampFilter"
    )
}

fn type_contains_geo_shape(
    type_name: &str,
    object_field_types: &HashMap<String, HashMap<String, String>>,
    memo: &mut HashMap<String, bool>,
    visiting: &mut std::collections::HashSet<String>,
) -> bool {
    if let Some(v) = memo.get(type_name) {
        return *v;
    }
    if !visiting.insert(type_name.to_string()) {
        return false;
    }

    let mut out = false;
    let mut paired_coordinates = (HashSet::new(), HashSet::new());

    if let Some(fields) = object_field_types.get(type_name) {
        let scalar_fields = fields
            .iter()
            .filter(|(_, child_type_name)| !object_field_types.contains_key(*child_type_name))
            .map(|(field_name, child_type_name)| (field_name.clone(), child_type_name.clone()))
            .collect::<HashMap<_, _>>();
        paired_coordinates = paired_numeric_coordinate_fields(&scalar_fields);
        for child_type_name in fields.values() {
            if object_field_types.contains_key(child_type_name)
                && type_contains_geo_shape(child_type_name, object_field_types, memo, visiting)
            {
                out = true;
                break;
            }
        }
    }

    visiting.remove(type_name);
    let result = out || (!paired_coordinates.0.is_empty() && !paired_coordinates.1.is_empty());
    memo.insert(type_name.to_string(), result);
    result
}

fn input_type_contains_time_field(
    type_name: &str,
    input_field_types: &HashMap<String, HashMap<String, String>>,
    memo: &mut HashMap<String, bool>,
    visiting: &mut std::collections::HashSet<String>,
) -> bool {
    if let Some(v) = memo.get(type_name) {
        return *v;
    }
    if !visiting.insert(type_name.to_string()) {
        return false;
    }
    let mut out = false;
    if let Some(fields) = input_field_types.get(type_name) {
        for nested_type_name in fields.values() {
            if is_temporal_scalar_type(nested_type_name)
                || is_temporal_filter_type(nested_type_name)
            {
                out = true;
                break;
            }
            if input_field_types.contains_key(nested_type_name)
                && input_type_contains_time_field(
                    nested_type_name,
                    input_field_types,
                    memo,
                    visiting,
                )
            {
                out = true;
                break;
            }
        }
    }
    visiting.remove(type_name);
    memo.insert(type_name.to_string(), out);
    out
}

fn push_unique(out: &mut Vec<String>, value: String) {
    if !out.iter().any(|v| v == &value) {
        out.push(value);
    }
}

fn override_role_list(
    target: &mut Vec<String>,
    fields: &HashMap<String, String>,
    candidates: &[String],
) {
    if candidates.is_empty() {
        return;
    }
    let mut filtered = candidates
        .iter()
        .filter(|field| fields.contains_key(*field))
        .cloned()
        .collect::<Vec<_>>();
    if filtered.is_empty() {
        return;
    }
    filtered.sort();
    filtered.dedup();
    *target = filtered;
}

fn apply_sls_role_overrides(
    roles: &mut FieldRoleSet,
    fields: &HashMap<String, String>,
    sls_roles: &FieldRoles,
) {
    override_role_list(
        &mut roles.entity_key_fields,
        fields,
        &sls_roles.entity_key_fields,
    );
    override_role_list(&mut roles.label_fields, fields, &sls_roles.label_fields);
    override_role_list(&mut roles.time_fields, fields, &sls_roles.time_fields);
    override_role_list(&mut roles.numeric_fields, fields, &sls_roles.numeric_fields);
    override_role_list(
        &mut roles.latitude_fields,
        fields,
        &sls_roles.latitude_fields,
    );
    override_role_list(
        &mut roles.longitude_fields,
        fields,
        &sls_roles.longitude_fields,
    );
    override_role_list(
        &mut roles.geo_object_fields,
        fields,
        &sls_roles.geo_object_fields,
    );
}

fn lookup_sls_role_map<'a>(
    map: &'a HashMap<String, FieldRoles>,
    key: &str,
) -> Option<&'a FieldRoles> {
    map.get(key).or_else(|| {
        map.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v)
    })
}

fn push_time_fields_from_roles(
    out: &mut Vec<String>,
    filter_fields: &HashMap<String, String>,
    roles: &FieldRoles,
) {
    for field in &roles.time_fields {
        if filter_fields.contains_key(field) {
            push_unique(out, field.clone());
        }
    }
}

fn candidate_field_names_from_arg(arg_name: &str) -> Vec<String> {
    let mut out = vec![arg_name.to_string()];
    if let Some(singular) = arg_name.strip_suffix('s')
        && !singular.is_empty()
        && !out.iter().any(|existing| existing == singular)
    {
        out.push(singular.to_string());
    }
    out
}

pub(crate) fn build_domain_config(
    object_field_types: &HashMap<String, HashMap<String, String>>,
    input_field_types: &HashMap<String, HashMap<String, String>>,
    query_filter_inputs: &HashMap<String, String>,
    query_return_types: &HashMap<String, String>,
    query_arg_types: &HashMap<String, HashMap<String, String>>,
    enum_values_map: &HashMap<String, Vec<String>>,
    sls: Option<&Sls>,
) -> DomainConfig {
    let mut id_fields = Vec::new();
    let mut entity_key_fields = Vec::new();
    let mut label_fields = Vec::new();
    let mut numeric_fields = Vec::new();
    let mut time_fields = Vec::new();
    let mut latitude_fields = Vec::new();
    let mut longitude_fields = Vec::new();
    let mut geo_object_fields = Vec::new();

    let sls_field_roles = sls.and_then(|s| s.field_roles.as_ref());
    let sls_roles_by_type = sls.map(|s| &s.field_roles_by_type);
    let sls_roles_by_root = sls.map(|s| &s.field_roles_by_root);
    let mut root_filter_object_fields = HashMap::new();
    let mut root_time_filter_fields = HashMap::new();
    let mut time_field_memo: HashMap<String, bool> = HashMap::new();
    let mut geo_shape_memo: HashMap<String, bool> = HashMap::new();
    for (root_field, filter_input) in query_filter_inputs {
        if let Some(fields) = input_field_types.get(filter_input) {
            let mut object_filter_fields = fields
                .iter()
                .filter_map(|(field, type_name)| {
                    if type_name.ends_with("Filter") {
                        Some(field.clone())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();
            object_filter_fields.sort();
            root_filter_object_fields.insert(root_field.clone(), object_filter_fields);

            let mut time_fields = fields
                .iter()
                .filter_map(|(field, type_name)| {
                    let mut visiting = std::collections::HashSet::new();
                    let nested_time = input_field_types.contains_key(type_name)
                        && input_type_contains_time_field(
                            type_name,
                            input_field_types,
                            &mut time_field_memo,
                            &mut visiting,
                        );
                    if is_temporal_scalar_type(type_name)
                        || is_temporal_filter_type(type_name)
                        || nested_time
                    {
                        Some(field.clone())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();
            if let Some(field_roles) = sls_field_roles {
                push_time_fields_from_roles(&mut time_fields, fields, field_roles);
            }
            if let Some(map) = sls_roles_by_root
                && let Some(root_roles) = lookup_sls_role_map(map, root_field)
            {
                push_time_fields_from_roles(&mut time_fields, fields, root_roles);
            }
            if let Some(return_type) = query_return_types.get(root_field)
                && let Some(map) = sls_roles_by_type
                && let Some(type_roles) = lookup_sls_role_map(map, return_type)
            {
                push_time_fields_from_roles(&mut time_fields, fields, type_roles);
            }
            time_fields.sort();
            time_fields.dedup();
            root_time_filter_fields.insert(root_field.clone(), time_fields);
        }
    }

    let mut roots_by_return_type: HashMap<String, Vec<String>> = HashMap::new();
    for (root_field, return_type) in query_return_types {
        roots_by_return_type
            .entry(return_type.clone())
            .or_default()
            .push(root_field.clone());
    }

    let mut type_field_roles: HashMap<String, FieldRoleSet> = HashMap::new();
    for (type_name, all_fields) in object_field_types {
        let mut roles = FieldRoleSet::default();
        let scalar_fields = all_fields
            .iter()
            .filter(|(_, field_type)| !object_field_types.contains_key(*field_type))
            .map(|(field_name, field_type)| (field_name.clone(), field_type.clone()))
            .collect::<HashMap<_, _>>();
        let roots = roots_by_return_type
            .get(type_name)
            .cloned()
            .unwrap_or_default();
        let mut direct_root_arg_fields = HashSet::new();
        let mut direct_lookup_fields = HashSet::new();
        let mut filterable_fields = HashSet::new();
        let mut filter_backed_time_fields = HashSet::new();
        let (paired_latitude_fields, paired_longitude_fields) =
            paired_numeric_coordinate_fields(&scalar_fields);

        for root_field in &roots {
            if let Some(arg_types) = query_arg_types.get(root_field) {
                for (arg_name, arg_type_name) in arg_types {
                    if input_field_types.contains_key(arg_type_name) {
                        continue;
                    }
                    for candidate in candidate_field_names_from_arg(arg_name) {
                        if scalar_fields.contains_key(&candidate) {
                            direct_root_arg_fields.insert(candidate.clone());
                            if root_field.starts_with("get") || root_field.starts_with("batchGet") {
                                direct_lookup_fields.insert(candidate);
                            }
                        }
                    }
                }
            }

            if root_field.starts_with("query")
                && let Some(filter_input) = query_filter_inputs.get(root_field)
                && let Some(filter_fields) = input_field_types.get(filter_input)
            {
                for field_name in filter_fields.keys() {
                    if scalar_fields.contains_key(field_name) {
                        filterable_fields.insert(field_name.clone());
                    }
                }
                if let Some(time_candidates) = root_time_filter_fields.get(root_field) {
                    for field_name in time_candidates {
                        if scalar_fields.contains_key(field_name) {
                            filter_backed_time_fields.insert(field_name.clone());
                        }
                    }
                }
            }
        }

        for (field_name, field_type) in &scalar_fields {
            if field_type == "ID" || direct_lookup_fields.contains(field_name) {
                push_unique(&mut roles.id_fields, field_name.clone());
            }
            if direct_root_arg_fields.contains(field_name) || filterable_fields.contains(field_name)
            {
                push_unique(&mut roles.entity_key_fields, field_name.clone());
            }
            if is_numeric_type_name(field_type) {
                push_unique(&mut roles.numeric_fields, field_name.clone());
            }
            if is_temporal_scalar_type(field_type) || filter_backed_time_fields.contains(field_name)
            {
                push_unique(&mut roles.time_fields, field_name.clone());
            }
            if paired_latitude_fields.contains(field_name) {
                push_unique(&mut roles.latitude_fields, field_name.clone());
            }
            if paired_longitude_fields.contains(field_name) {
                push_unique(&mut roles.longitude_fields, field_name.clone());
            }
        }

        for (field_name, field_type) in all_fields {
            if !object_field_types.contains_key(field_type) {
                continue;
            }
            let mut visiting = std::collections::HashSet::new();
            if type_contains_geo_shape(
                field_type,
                object_field_types,
                &mut geo_shape_memo,
                &mut visiting,
            ) {
                push_unique(&mut roles.geo_object_fields, field_name.clone());
            }
        }

        let mut label_candidates = filterable_fields
            .iter()
            .filter(|field_name| {
                scalar_fields
                    .get(*field_name)
                    .is_some_and(|field_type| is_string_like_type_name(field_type))
                    && !roles
                        .id_fields
                        .iter()
                        .any(|existing| existing == *field_name)
            })
            .cloned()
            .collect::<Vec<_>>();
        if label_candidates.is_empty() {
            label_candidates = direct_root_arg_fields
                .iter()
                .filter(|field_name| {
                    scalar_fields
                        .get(*field_name)
                        .is_some_and(|field_type| is_string_like_type_name(field_type))
                        && !roles
                            .id_fields
                            .iter()
                            .any(|existing| existing == *field_name)
                })
                .cloned()
                .collect::<Vec<_>>();
        }
        label_candidates.sort();
        for field in label_candidates {
            push_unique(&mut roles.label_fields, field);
        }

        if let Some(field_roles) = sls_field_roles {
            for field in &field_roles.entity_key_fields {
                if all_fields.contains_key(field) {
                    push_unique(&mut roles.entity_key_fields, field.clone());
                }
            }
            for field in &field_roles.label_fields {
                if all_fields.contains_key(field) {
                    push_unique(&mut roles.label_fields, field.clone());
                }
            }
            for field in &field_roles.time_fields {
                if all_fields.contains_key(field) {
                    push_unique(&mut roles.time_fields, field.clone());
                }
            }
            for field in &field_roles.numeric_fields {
                if all_fields.contains_key(field) {
                    push_unique(&mut roles.numeric_fields, field.clone());
                }
            }
            for field in &field_roles.latitude_fields {
                if all_fields.contains_key(field) {
                    push_unique(&mut roles.latitude_fields, field.clone());
                }
            }
            for field in &field_roles.longitude_fields {
                if all_fields.contains_key(field) {
                    push_unique(&mut roles.longitude_fields, field.clone());
                }
            }
            for field in &field_roles.geo_object_fields {
                if all_fields.contains_key(field) {
                    push_unique(&mut roles.geo_object_fields, field.clone());
                }
            }
        }
        if let Some(map) = sls_roles_by_type
            && let Some(type_roles) = lookup_sls_role_map(map, type_name)
        {
            apply_sls_role_overrides(&mut roles, all_fields, type_roles);
        }
        roles.id_fields.sort();
        roles.entity_key_fields.sort();
        roles.label_fields.sort();
        roles.numeric_fields.sort();
        roles.time_fields.sort();
        roles.latitude_fields.sort();
        roles.longitude_fields.sort();
        roles.geo_object_fields.sort();
        type_field_roles.insert(type_name.clone(), roles);
    }

    let mut root_field_roles: HashMap<String, FieldRoleSet> = HashMap::new();
    for (root_field, return_type) in query_return_types {
        if let Some(mut roles) = type_field_roles.get(return_type).cloned() {
            if let Some(map) = sls_roles_by_root
                && let Some(root_roles) = lookup_sls_role_map(map, root_field)
                && let Some(fields) = object_field_types.get(return_type)
            {
                apply_sls_role_overrides(&mut roles, fields, root_roles);
            }
            root_field_roles.insert(root_field.clone(), roles);
        }
    }

    let mut root_identifier_filter_fields = HashMap::new();
    for (root_field, filter_input) in query_filter_inputs {
        if let Some(fields) = input_field_types.get(filter_input) {
            let role_candidates = root_field_roles
                .get(root_field)
                .cloned()
                .unwrap_or_default();
            let role_candidate_set = role_candidates
                .id_fields
                .iter()
                .chain(role_candidates.entity_key_fields.iter())
                .chain(role_candidates.label_fields.iter())
                .cloned()
                .collect::<HashSet<_>>();
            let mut identifier_candidates = fields
                .keys()
                .filter(|field| role_candidate_set.contains(*field))
                .cloned()
                .collect::<Vec<_>>();
            if identifier_candidates.is_empty()
                && let Some(return_type) = query_return_types.get(root_field)
                && let Some(return_fields) = object_field_types.get(return_type)
            {
                identifier_candidates = fields
                    .iter()
                    .filter_map(|(field, type_name)| {
                        if !return_fields.contains_key(field)
                            || !is_string_like_type_name(type_name)
                            || input_field_types.contains_key(type_name)
                        {
                            return None;
                        }
                        Some(field.clone())
                    })
                    .collect::<Vec<_>>();
            }
            identifier_candidates.sort();
            root_identifier_filter_fields.insert(root_field.clone(), identifier_candidates);
        }
    }

    for roles in type_field_roles.values() {
        for field in &roles.id_fields {
            push_unique(&mut id_fields, field.clone());
        }
        for field in &roles.entity_key_fields {
            push_unique(&mut entity_key_fields, field.clone());
        }
        for field in &roles.label_fields {
            push_unique(&mut label_fields, field.clone());
        }
        for field in &roles.numeric_fields {
            push_unique(&mut numeric_fields, field.clone());
        }
        for field in &roles.time_fields {
            push_unique(&mut time_fields, field.clone());
        }
        for field in &roles.latitude_fields {
            push_unique(&mut latitude_fields, field.clone());
        }
        for field in &roles.longitude_fields {
            push_unique(&mut longitude_fields, field.clone());
        }
        for field in &roles.geo_object_fields {
            push_unique(&mut geo_object_fields, field.clone());
        }
    }

    if let Some(field_roles) = sls_field_roles {
        for field in &field_roles.entity_key_fields {
            push_unique(&mut entity_key_fields, field.clone());
        }
        for field in &field_roles.label_fields {
            push_unique(&mut label_fields, field.clone());
        }
        for field in &field_roles.time_fields {
            push_unique(&mut time_fields, field.clone());
        }
        for field in &field_roles.numeric_fields {
            push_unique(&mut numeric_fields, field.clone());
        }
        for field in &field_roles.latitude_fields {
            push_unique(&mut latitude_fields, field.clone());
        }
        for field in &field_roles.longitude_fields {
            push_unique(&mut longitude_fields, field.clone());
        }
        for field in &field_roles.geo_object_fields {
            push_unique(&mut geo_object_fields, field.clone());
        }
    }

    id_fields.sort();
    entity_key_fields.sort();
    label_fields.sort();
    numeric_fields.sort();
    time_fields.sort();
    latitude_fields.sort();
    longitude_fields.sort();
    geo_object_fields.sort();

    let mut enum_values = HashMap::new();
    let mut enum_names = enum_values_map.keys().cloned().collect::<Vec<_>>();
    enum_names.sort();
    for enum_name in enum_names {
        if let Some(values) = enum_values_map.get(&enum_name) {
            enum_values.insert(enum_name, values.clone());
        }
    }

    DomainConfig {
        id_fields,
        entity_key_fields,
        label_fields,
        numeric_fields,
        time_fields,
        location_fields: FieldPosition {
            latitude_fields,
            longitude_fields,
            geo_object_fields,
        },
        enum_values,
        root_identifier_filter_fields,
        root_filter_object_fields,
        root_time_filter_fields,
        type_field_roles,
        root_field_roles,
    }
}
