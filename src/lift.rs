//! Generic, mapping-free lifting of non-RDF sources into a structural RDF graph.
//! Model-specific transformation is then done with SHACL-AF rules.

use std::path::Path;

use anyhow::{Context, Result, bail};
use oxigraph::io::RdfFormat;
use oxigraph::model::*;
use oxigraph::store::Store;
use serde_json::Value;

use crate::graph;
use crate::vocab::{JSON_NS, qs, rdf, xsd};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceFormat {
    Json,
    Csv,
    Rdf(RdfFormat),
}

impl SourceFormat {
    pub fn detect(path: &Path, explicit: Option<&str>) -> Result<Self> {
        let name = match explicit {
            Some(f) => f.to_owned(),
            None => path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or_default()
                .to_lowercase(),
        };
        Ok(match name.as_str() {
            "json" => Self::Json,
            "csv" => Self::Csv,
            other => Self::Rdf(
                graph::parse_format(other)
                    .with_context(|| format!("cannot infer source format of {}", path.display()))?,
            ),
        })
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct LiftOptions {
    /// Number of CSV rows to lift as `qs:Row` nodes.
    pub rows: usize,
}

/// Lifts a file into a fresh in-memory graph.
pub fn lift_file(path: &Path, format: SourceFormat, options: LiftOptions) -> Result<Store> {
    let bytes = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    let g = graph::new_store();
    let triples = match format {
        SourceFormat::Rdf(f) => {
            graph::load(&g, &bytes, f)?;
            return Ok(g);
        }
        SourceFormat::Json => {
            let value: Value = serde_json::from_slice(&bytes).context("invalid JSON")?;
            lift_json(&value)
        }
        SourceFormat::Csv => lift_csv(&bytes, options)?,
    };
    let doc = triples[0].subject.clone();
    for t in triples
        .iter()
        .chain(&file_metadata(&doc, path, bytes.len()))
    {
        graph::insert(&g, t)?;
    }
    Ok(g)
}

fn file_metadata(doc: &NamedOrBlankNode, path: &Path, size: usize) -> Vec<Triple> {
    let mut out = vec![
        Triple::new(
            doc.clone(),
            qs::SOURCE_FILE,
            Literal::from(path.display().to_string()),
        ),
        Triple::new(doc.clone(), qs::FILE_SIZE_BYTES, Literal::from(size as i64)),
    ];
    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
        out.push(Triple::new(doc.clone(), qs::FILE_NAME, Literal::from(stem)));
    }
    if let Some(ms) = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
    {
        let iso = graph::epoch_millis_to_iso(ms.as_millis() as i64);
        out.push(Triple::new(
            doc.clone(),
            qs::MODIFIED,
            Literal::new_typed_literal(iso, xsd::DATE_TIME),
        ));
    }
    out
}

pub fn json_key(key: &str) -> NamedNode {
    let mut iri = String::from(JSON_NS);
    for b in key.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            iri.push(b as char);
        } else {
            iri.push_str(&format!("%{b:02X}"));
        }
    }
    NamedNode::new_unchecked(iri)
}

/// Lifts JSON: objects become blank nodes, keys become `j:` predicates, array members are
/// repeated values (object members carry `qs:index`). The first triple's subject is the document.
pub fn lift_json(value: &Value) -> Vec<Triple> {
    let doc: NamedOrBlankNode = BlankNode::default().into();
    let mut out = vec![
        Triple::new(doc.clone(), rdf::TYPE, qs::DOCUMENT),
        Triple::new(doc.clone(), qs::FORMAT, Literal::from("json")),
    ];
    match value {
        Value::Object(map) => lift_object(&doc, map, &mut out),
        other => lift_value(&doc, qs::ITEM.into_owned(), other, None, &mut out),
    }
    out
}

fn lift_object(
    node: &NamedOrBlankNode,
    map: &serde_json::Map<String, Value>,
    out: &mut Vec<Triple>,
) {
    for (k, v) in map {
        lift_value(node, json_key(k), v, None, out);
    }
}

fn scalar(v: &Value) -> Option<Literal> {
    Some(match v {
        Value::Bool(b) => Literal::from(*b),
        Value::Number(n) => match (n.as_i64(), n.as_u64(), n.as_f64()) {
            (Some(i), _, _) => Literal::from(i),
            (None, Some(u), _) => Literal::new_typed_literal(u.to_string(), xsd::INTEGER),
            (_, _, Some(f)) => Literal::from(f),
            _ => return None,
        },
        Value::String(s) => Literal::from(s.as_str()),
        _ => return None,
    })
}

fn lift_value(
    subject: &NamedOrBlankNode,
    predicate: NamedNode,
    v: &Value,
    index: Option<usize>,
    out: &mut Vec<Triple>,
) {
    match v {
        Value::Null => {}
        Value::Object(map) => {
            let child: NamedOrBlankNode = BlankNode::default().into();
            out.push(Triple::new(subject.clone(), predicate, child.clone()));
            if let Some(i) = index {
                out.push(Triple::new(
                    child.clone(),
                    qs::INDEX,
                    Literal::from(i as i64),
                ));
            }
            lift_object(&child, map, out);
        }
        Value::Array(items) => {
            if let Some(i) = index {
                let child: NamedOrBlankNode = BlankNode::default().into();
                out.push(Triple::new(subject.clone(), predicate, child.clone()));
                out.push(Triple::new(child.clone(), rdf::TYPE, qs::ARRAY));
                out.push(Triple::new(
                    child.clone(),
                    qs::INDEX,
                    Literal::from(i as i64),
                ));
                for (j, item) in items.iter().enumerate() {
                    lift_value(&child, qs::ITEM.into_owned(), item, Some(j), out);
                }
            } else {
                for (j, item) in items.iter().enumerate() {
                    lift_value(subject, predicate.clone(), item, Some(j), out);
                }
            }
        }
        scalar_value => {
            if let Some(l) = scalar(scalar_value) {
                out.push(Triple::new(subject.clone(), predicate, l));
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Inferred {
    Boolean,
    Integer,
    Double,
    Date,
    DateTime,
    String,
}

impl Inferred {
    fn datatype(self) -> NamedNodeRef<'static> {
        match self {
            Self::Boolean => xsd::BOOLEAN,
            Self::Integer => xsd::INTEGER,
            Self::Double => xsd::DOUBLE,
            Self::Date => xsd::DATE,
            Self::DateTime => xsd::DATE_TIME,
            Self::String => xsd::STRING,
        }
    }

    fn accepts(self, v: &str) -> bool {
        use std::str::FromStr;
        match self {
            Self::Boolean => matches!(v, "true" | "false"),
            Self::Integer => v.parse::<i64>().is_ok(),
            Self::Double => v.parse::<f64>().is_ok(),
            Self::Date => oxsdatatypes::Date::from_str(v).is_ok(),
            Self::DateTime => oxsdatatypes::DateTime::from_str(v).is_ok(),
            Self::String => true,
        }
    }

    fn literal(self, v: &str) -> Literal {
        if self == Self::String {
            Literal::from(v)
        } else {
            Literal::new_typed_literal(v, self.datatype())
        }
    }

    fn less(self, a: &str, b: &str) -> bool {
        match self {
            Self::Integer => a.parse::<i64>().ok() < b.parse::<i64>().ok(),
            Self::Double => {
                a.parse::<f64>().unwrap_or(f64::NAN) < b.parse::<f64>().unwrap_or(f64::NAN)
            }
            _ => a < b,
        }
    }
}

const CANDIDATES: [Inferred; 6] = [
    Inferred::Boolean,
    Inferred::Integer,
    Inferred::Double,
    Inferred::Date,
    Inferred::DateTime,
    Inferred::String,
];

/// Lifts CSV: one `qs:Column` per header with inferred type, null count and min/max,
/// plus up to `options.rows` `qs:Row` nodes.
pub fn lift_csv(bytes: &[u8], options: LiftOptions) -> Result<Vec<Triple>> {
    let mut reader = csv::Reader::from_reader(bytes);
    let headers: Vec<String> = reader.headers()?.iter().map(str::to_owned).collect();
    if headers.is_empty() {
        bail!("CSV file has no header");
    }
    let mut records = Vec::new();
    for r in reader.records() {
        records.push(r?);
    }
    let doc: NamedOrBlankNode = BlankNode::default().into();
    let mut out = vec![
        Triple::new(doc.clone(), rdf::TYPE, qs::DOCUMENT),
        Triple::new(doc.clone(), qs::FORMAT, Literal::from("csv")),
        Triple::new(
            doc.clone(),
            qs::ROW_COUNT,
            Literal::from(records.len() as i64),
        ),
    ];
    let mut types = Vec::new();
    for (i, name) in headers.iter().enumerate() {
        let values: Vec<&str> = records
            .iter()
            .filter_map(|r| r.get(i))
            .filter(|v| !v.is_empty())
            .collect();
        let inferred = CANDIDATES
            .into_iter()
            .find(|t| !values.is_empty() && values.iter().all(|v| t.accepts(v)))
            .unwrap_or(Inferred::String);
        types.push(inferred);
        let col: NamedOrBlankNode = BlankNode::default().into();
        out.push(Triple::new(doc.clone(), qs::COLUMN, col.clone()));
        out.push(Triple::new(col.clone(), rdf::TYPE, qs::COLUMN_CLASS));
        out.push(Triple::new(
            col.clone(),
            qs::NAME,
            Literal::from(name.as_str()),
        ));
        out.push(Triple::new(
            col.clone(),
            qs::POSITION,
            Literal::from(i as i64 + 1),
        ));
        out.push(Triple::new(
            col.clone(),
            qs::INFERRED_TYPE,
            inferred.datatype(),
        ));
        out.push(Triple::new(
            col.clone(),
            qs::NULL_COUNT,
            Literal::from((records.len() - values.len()) as i64),
        ));
        let mut min: Option<&str> = None;
        let mut max: Option<&str> = None;
        for v in &values {
            if min.is_none_or(|m| inferred.less(v, m)) {
                min = Some(v);
            }
            if max.is_none_or(|m| inferred.less(m, v)) {
                max = Some(v);
            }
        }
        if let (Some(min), Some(max)) = (min, max) {
            out.push(Triple::new(
                col.clone(),
                qs::MIN_VALUE,
                inferred.literal(min),
            ));
            out.push(Triple::new(
                col.clone(),
                qs::MAX_VALUE,
                inferred.literal(max),
            ));
        }
    }
    for (i, record) in records.iter().take(options.rows).enumerate() {
        let row: NamedOrBlankNode = BlankNode::default().into();
        out.push(Triple::new(doc.clone(), qs::ROW, row.clone()));
        out.push(Triple::new(row.clone(), rdf::TYPE, qs::ROW_CLASS));
        out.push(Triple::new(row.clone(), qs::INDEX, Literal::from(i as i64)));
        for (j, value) in record.iter().enumerate() {
            if !value.is_empty() {
                out.push(Triple::new(
                    row.clone(),
                    json_key(&headers[j]),
                    types[j].literal(value),
                ));
            }
        }
    }
    Ok(out)
}
