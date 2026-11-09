use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::Args;
use comfy_table::{presets::UTF8_FULL_CONDENSED, Table};
use serde_json::{json, Value};

use crate::api::{ApiClient, Fusion, QueryRequest};
use crate::commands::load::parse_vector;
use crate::output;

#[derive(Args)]
pub struct QueryArgs {
    /// Namespace to query
    #[arg(short = 'n', long)]
    pub namespace: String,

    /// Full-text query (BM25)
    #[arg(long)]
    pub text: Option<String>,

    /// Dense query vector as comma-separated floats, e.g. --vector 0.1,0.2,0.3
    #[arg(long, value_delimiter = ',')]
    pub vector: Option<Vec<f32>>,

    /// Read the dense query vector from a file containing a JSON array
    #[arg(long, conflicts_with = "vector")]
    pub vector_file: Option<PathBuf>,

    /// Number of results to return
    #[arg(short = 'k', long, default_value_t = 10)]
    pub top_k: usize,

    /// Raw filter expression as JSON (see docs/query-language.md)
    #[arg(long)]
    pub filter: Option<String>,

    /// Simple filter conditions, repeatable; ANDed together.
    /// Examples: --where 'genre=fiction' --where 'year>=2000' --where 'tag in a,b,c'
    #[arg(long = "where", value_name = "CONDITION")]
    pub conditions: Vec<String>,

    /// Only return these attributes (comma-separated). Default: all attributes.
    #[arg(long, value_delimiter = ',')]
    pub include: Option<Vec<String>>,

    /// Include document vectors in results
    #[arg(long)]
    pub include_vectors: bool,

    /// Execution mode: auto | exact | ann
    #[arg(long)]
    pub mode: Option<String>,

    /// Hybrid fusion method when both --text and --vector are given: rrf | weighted
    #[arg(long)]
    pub fusion: Option<String>,

    /// Weight for the vector leg of weighted fusion (0.0 - 1.0)
    #[arg(long)]
    pub vector_weight: Option<f32>,

    /// Weight for the text leg of weighted fusion (0.0 - 1.0)
    #[arg(long)]
    pub text_weight: Option<f32>,
}

pub async fn run(client: &ApiClient, args: QueryArgs, json_mode: bool) -> Result<()> {
    let req = build_request(&args)?;
    let resp = client.query(&args.namespace, &req).await?;

    if json_mode {
        let v = json!({
            "results": resp.results.iter().map(|r| {
                let mut obj = json!({ "id": r.id, "score": r.score, "attributes": r.attributes });
                if let Some(vec) = &r.vector {
                    obj["vector"] = json!(vec);
                }
                obj
            }).collect::<Vec<_>>(),
            "took_ms": resp.took_ms,
            "plan": resp.plan,
        });
        output::print_json(&v);
        return Ok(());
    }

    if resp.results.is_empty() {
        println!("no results");
    } else {
        let mut table = Table::new();
        table.load_preset(UTF8_FULL_CONDENSED);
        table.set_header(vec!["#", "id", "score", "attributes"]);
        for (i, r) in resp.results.iter().enumerate() {
            let attrs = serde_json::to_string(&r.attributes).unwrap_or_default();
            table.add_row(vec![
                (i + 1).to_string(),
                r.id.clone(),
                format!("{:.4}", r.score),
                output::truncate(&attrs, 80),
            ]);
        }
        println!("{table}");
    }
    if let Some(ms) = resp.took_ms {
        println!("took {ms:.2} ms");
    }
    Ok(())
}

fn build_request(args: &QueryArgs) -> Result<QueryRequest> {
    let vector = match (&args.vector, &args.vector_file) {
        (Some(v), _) => Some(v.clone()),
        (None, Some(path)) => {
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            let value: Value = serde_json::from_str(&raw).context("parsing vector file")?;
            Some(parse_vector(&value)?)
        }
        (None, None) => None,
    };

    if vector.is_none() && args.text.is_none() && args.filter.is_none() && args.conditions.is_empty()
    {
        bail!("provide at least one of --text, --vector/--vector-file, --filter, or --where");
    }

    let filter = build_filter(args.filter.as_deref(), &args.conditions)?;

    let fusion = if vector.is_some() && args.text.is_some() {
        let method = args.fusion.clone().unwrap_or_else(|| "rrf".to_string());
        if method != "rrf" && method != "weighted" {
            bail!("--fusion must be 'rrf' or 'weighted'");
        }
        Some(Fusion {
            method,
            vector_weight: args.vector_weight,
            text_weight: args.text_weight,
        })
    } else {
        None
    };

    let include_attributes = match &args.include {
        Some(fields) => Some(json!(fields)),
        None => Some(Value::Bool(true)),
    };

    Ok(QueryRequest {
        top_k: args.top_k,
        vector,
        text: args.text.clone(),
        sparse_vector: None,
        filter,
        include_attributes,
        include_vectors: args.include_vectors,
        mode: args.mode.clone(),
        fusion,
    })
}

pub fn build_filter(raw: Option<&str>, conditions: &[String]) -> Result<Option<Value>> {
    let mut clauses: Vec<Value> = Vec::new();
    if let Some(raw) = raw {
        let parsed: Value = serde_json::from_str(raw).context("parsing --filter JSON")?;
        clauses.push(parsed);
    }
    for cond in conditions {
        clauses.push(parse_condition(cond)?);
    }
    Ok(match clauses.len() {
        0 => None,
        1 => Some(clauses.into_iter().next().expect("one clause")),
        _ => Some(json!({ "op": "and", "filters": clauses })),
    })
}

/// Parse a simple `--where` condition into a Lakefin filter object.
///
/// Supported syntaxes (checked in this order so multi-char operators win):
///   field!=value   field>=value   field<=value
///   field=value    field>value    field<value
///   field in a,b,c
///   field contains_any a,b,c
pub fn parse_condition(cond: &str) -> Result<Value> {
    let trimmed = cond.trim();

    // Word operators: "field in a,b,c" / "field contains_any a,b"
    for (word, op) in [(" in ", "in"), (" contains_any ", "contains_any")] {
        if let Some(pos) = trimmed.find(word) {
            let field = trimmed[..pos].trim();
            let list = trimmed[pos + word.len()..].trim();
            if field.is_empty() || list.is_empty() {
                bail!("invalid condition: '{cond}'");
            }
            let values: Vec<Value> = list.split(',').map(|p| coerce(p.trim())).collect();
            return Ok(json!({ "field": field, "op": op, "value": values }));
        }
    }

    // Symbol operators, longest first.
    for (sym, op) in [
        ("!=", "not_eq"),
        (">=", "gte"),
        ("<=", "lte"),
        ("=", "eq"),
        (">", "gt"),
        ("<", "lt"),
    ] {
        if let Some(pos) = trimmed.find(sym) {
            let field = trimmed[..pos].trim();
            let value = trimmed[pos + sym.len()..].trim();
            if field.is_empty() || value.is_empty() {
                bail!("invalid condition: '{cond}'");
            }
            return Ok(json!({ "field": field, "op": op, "value": coerce(value) }));
        }
    }

    bail!(
        "could not parse condition '{cond}'; expected forms like field=value, \
         field>=value, or 'field in a,b,c'"
    )
}

fn coerce(s: &str) -> Value {
    if let Ok(i) = s.parse::<i64>() {
        return json!(i);
    }
    if let Ok(f) = s.parse::<f64>() {
        if let Some(n) = serde_json::Number::from_f64(f) {
            return Value::Number(n);
        }
    }
    match s {
        "true" => return Value::Bool(true),
        "false" => return Value::Bool(false),
        _ => {}
    }
    // Allow quoting to force string interpretation of numeric-looking values.
    let unquoted = s
        .strip_prefix('"')
        .and_then(|x| x.strip_suffix('"'))
        .or_else(|| s.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')));
    Value::String(unquoted.unwrap_or(s).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eq_condition() {
        let f = parse_condition("genre=fiction").unwrap();
        assert_eq!(f, json!({"field": "genre", "op": "eq", "value": "fiction"}));
    }

    #[test]
    fn not_eq_beats_eq() {
        let f = parse_condition("genre!=fiction").unwrap();
        assert_eq!(f["op"], "not_eq");
    }

    #[test]
    fn numeric_coercion_and_range_ops() {
        let f = parse_condition("year>=2000").unwrap();
        assert_eq!(f, json!({"field": "year", "op": "gte", "value": 2000}));
        let f = parse_condition("score<0.5").unwrap();
        assert_eq!(f["op"], "lt");
        assert_eq!(f["value"], json!(0.5));
    }

    #[test]
    fn in_condition() {
        let f = parse_condition("tag in a,b,c").unwrap();
        assert_eq!(
            f,
            json!({"field": "tag", "op": "in", "value": ["a", "b", "c"]})
        );
    }

    #[test]
    fn contains_any_condition() {
        let f = parse_condition("labels contains_any red,blue").unwrap();
        assert_eq!(f["op"], "contains_any");
    }

    #[test]
    fn quoted_value_stays_string() {
        let f = parse_condition("zip='02134'").unwrap();
        assert_eq!(f["value"], json!("02134"));
    }

    #[test]
    fn multiple_conditions_become_and() {
        let f = build_filter(None, &["a=1".into(), "b=2".into()])
            .unwrap()
            .unwrap();
        assert_eq!(f["op"], "and");
        assert_eq!(f["filters"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn single_condition_stays_flat() {
        let f = build_filter(None, &["a=1".into()]).unwrap().unwrap();
        assert_eq!(f["op"], "eq");
    }

    #[test]
    fn raw_filter_merges_with_conditions() {
        let f = build_filter(
            Some(r#"{"field":"x","op":"eq","value":1}"#),
            &["y=2".into()],
        )
        .unwrap()
        .unwrap();
        assert_eq!(f["op"], "and");
    }
}
