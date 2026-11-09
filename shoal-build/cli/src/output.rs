use std::collections::BTreeSet;

use comfy_table::{presets::UTF8_FULL_CONDENSED, Table};
use serde_json::Value;

pub fn print_json(v: &Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(v).expect("serializing JSON value")
    );
}

/// Human-friendly rendering: arrays of objects become tables, everything else
/// is pretty-printed JSON. `--json` forces raw JSON output.
pub fn print_value(json_mode: bool, v: &Value) {
    if json_mode {
        print_json(v);
        return;
    }
    match v {
        Value::Array(rows) if !rows.is_empty() && rows.iter().all(Value::is_object) => {
            print_object_table(rows)
        }
        Value::Array(rows) if rows.is_empty() => println!("(empty)"),
        _ => print_json(v),
    }
}

pub fn print_object_table(rows: &[Value]) {
    let mut columns: BTreeSet<String> = BTreeSet::new();
    for row in rows {
        if let Some(obj) = row.as_object() {
            for key in obj.keys() {
                columns.insert(key.clone());
            }
        }
    }
    let columns: Vec<String> = columns.into_iter().collect();

    let mut table = Table::new();
    table.load_preset(UTF8_FULL_CONDENSED);
    table.set_header(columns.clone());
    for row in rows {
        let obj = row.as_object().expect("checked object rows");
        let cells: Vec<String> = columns
            .iter()
            .map(|c| cell_string(obj.get(c)))
            .collect();
        table.add_row(cells);
    }
    println!("{table}");
}

fn cell_string(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => truncate(s, 60),
        Some(other) => truncate(&other.to_string(), 60),
    }
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short_passthrough() {
        assert_eq!(truncate("abc", 10), "abc");
    }

    #[test]
    fn truncate_long_appends_ellipsis() {
        let t = truncate("abcdefghij", 5);
        assert_eq!(t.chars().count(), 5);
        assert!(t.ends_with('…'));
    }
}
