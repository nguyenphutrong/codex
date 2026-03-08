use crate::types::LspDiagnostic;
use crate::types::LspPosition;
use crate::types::LspRange;
use crate::util::uri_to_path;
use serde_json::Value;
use serde_json::json;
use std::path::Path;

pub(crate) fn diagnostic_from_value(path: &Path, value: &Value) -> Option<LspDiagnostic> {
    Some(LspDiagnostic {
        server: None,
        path: path.to_path_buf(),
        range: range_from_value(value.get("range")?)?,
        severity: value
            .get("severity")
            .and_then(Value::as_u64)
            .and_then(|severity| u8::try_from(severity).ok()),
        message: value.get("message")?.as_str()?.to_string(),
        source: value
            .get("source")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

pub(crate) fn normalize_location_like(value: &Value) -> Vec<Value> {
    if value.is_null() {
        return Vec::new();
    }

    match value {
        Value::Array(items) => items
            .iter()
            .filter_map(normalize_location_or_link)
            .collect(),
        _ => normalize_location_or_link(value).into_iter().collect(),
    }
}

fn normalize_location_or_link(value: &Value) -> Option<Value> {
    if value.get("uri").is_some() {
        return Some(json!({
            "path": uri_to_path(value.get("uri")?.as_str()?).ok()?,
            "range": range_to_value(&range_from_value(value.get("range")?)?),
        }));
    }

    let range = value
        .get("targetSelectionRange")
        .or_else(|| value.get("targetRange"))?;
    Some(json!({
        "path": uri_to_path(value.get("targetUri")?.as_str()?).ok()?,
        "range": range_to_value(&range_from_value(range)?),
    }))
}

pub(crate) fn normalize_hover(value: &Value) -> Vec<Value> {
    if value.is_null() {
        return Vec::new();
    }

    let contents = flatten_hover_contents(value.get("contents"));
    if contents.is_empty() {
        return Vec::new();
    }

    let mut item = json!({
        "contents": contents,
    });
    if let Some(range) = value.get("range").and_then(range_from_value) {
        item["range"] = range_to_value(&range);
    }
    vec![item]
}

fn flatten_hover_contents(value: Option<&Value>) -> String {
    let Some(value) = value else {
        return String::new();
    };

    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(|item| flatten_hover_contents(Some(item)))
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
        Value::Object(object) => object
            .get("value")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                object
                    .get("language")
                    .and_then(|_| serde_json::to_string(object).ok())
            })
            .unwrap_or_default(),
        _ => String::new(),
    }
}

pub(crate) fn normalize_symbol_information(value: &Value) -> Vec<Value> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            Some(json!({
                "name": item.get("name")?.as_str()?,
                "kind": item.get("kind")?.as_i64()?,
                "detail": item.get("detail").and_then(Value::as_str),
                "container_name": item.get("containerName").and_then(Value::as_str),
                "location": item.get("location").and_then(normalize_location_or_link),
            }))
        })
        .collect()
}

pub(crate) fn normalize_document_symbols(value: &Value) -> Vec<Value> {
    let Some(items) = value.as_array() else {
        return Vec::new();
    };

    if items
        .first()
        .and_then(|item| item.get("location"))
        .is_some()
    {
        return normalize_symbol_information(value);
    }

    let mut output = Vec::new();
    for item in items {
        flatten_document_symbol(item, None, &mut output);
    }
    output
}

fn flatten_document_symbol(item: &Value, container_name: Option<&str>, output: &mut Vec<Value>) {
    let Some(name) = item.get("name").and_then(Value::as_str) else {
        return;
    };
    let Some(kind) = item.get("kind").and_then(Value::as_i64) else {
        return;
    };
    let Some(range) = item.get("range").and_then(range_from_value) else {
        return;
    };

    output.push(json!({
        "name": name,
        "kind": kind,
        "detail": item.get("detail").and_then(Value::as_str),
        "container_name": container_name,
        "location": {
            "range": range_to_value(&range),
        },
    }));

    if let Some(children) = item.get("children").and_then(Value::as_array) {
        for child in children {
            flatten_document_symbol(child, Some(name), output);
        }
    }
}

pub(crate) fn normalize_call_hierarchy_items(value: &Value) -> Vec<Value> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(call_hierarchy_item)
        .collect()
}

pub(crate) fn normalize_call_hierarchy_calls(value: &Value) -> Vec<Value> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let item = entry
                .get("from")
                .or_else(|| entry.get("to"))
                .and_then(call_hierarchy_item)?;
            let mut item = item;
            if let Some(ranges) = entry
                .get("fromRanges")
                .and_then(Value::as_array)
                .map(|ranges| {
                    ranges
                        .iter()
                        .filter_map(range_from_value)
                        .map(|range| range_to_value(&range))
                        .collect::<Vec<_>>()
                })
            {
                item["from_ranges"] = Value::Array(ranges);
            }
            if let Some(ranges) = entry
                .get("toRanges")
                .and_then(Value::as_array)
                .map(|ranges| {
                    ranges
                        .iter()
                        .filter_map(range_from_value)
                        .map(|range| range_to_value(&range))
                        .collect::<Vec<_>>()
                })
            {
                item["to_ranges"] = Value::Array(ranges);
            }
            Some(item)
        })
        .collect()
}

fn call_hierarchy_item(value: &Value) -> Option<Value> {
    Some(json!({
        "name": value.get("name")?.as_str()?,
        "kind": value.get("kind")?.as_i64()?,
        "path": uri_to_path(value.get("uri")?.as_str()?).ok()?,
        "selection_range": range_to_value(&range_from_value(value.get("selectionRange")?)?),
    }))
}

pub(crate) fn range_from_value(value: &Value) -> Option<LspRange> {
    Some(LspRange {
        start: position_from_value(value.get("start")?)?,
        end: position_from_value(value.get("end")?)?,
    })
}

fn position_from_value(value: &Value) -> Option<LspPosition> {
    let line = value.get("line")?.as_u64()?;
    let character = value.get("character")?.as_u64()?;
    Some(LspPosition {
        line: usize::try_from(line).ok()?.saturating_add(1),
        character: usize::try_from(character).ok()?.saturating_add(1),
    })
}

pub(crate) fn range_to_value(range: &LspRange) -> Value {
    json!({
        "start": {
            "line": range.start.line,
            "character": range.start.character,
        },
        "end": {
            "line": range.end.line,
            "character": range.end.character,
        },
    })
}
