//! Small helpers over in-memory oxigraph stores, used as RDF graphs (default graph only).

use std::collections::HashSet;
use std::str::FromStr;
use std::sync::{Arc, LazyLock};

use anyhow::{Context, Result, anyhow};
use oxigraph::io::{RdfFormat, RdfParser, RdfSerializer};
use oxigraph::model::*;
use oxigraph::sparql::{QueryResults, QuerySolution, SparqlEvaluator, Variable};
use oxigraph::store::Store;

use crate::vocab::{PREFIXES, qf, rdf, rdfs, xsd};

pub fn new_store() -> Store {
    Store::new().expect("in-memory store creation cannot fail")
}

pub fn as_subject(term: &Term) -> Option<NamedOrBlankNodeRef<'_>> {
    match term {
        Term::NamedNode(n) => Some(n.as_ref().into()),
        Term::BlankNode(b) => Some(b.as_ref().into()),
        _ => None,
    }
}

fn default_graph(
    g: &Store,
    s: Option<NamedOrBlankNodeRef<'_>>,
    p: Option<NamedNodeRef<'_>>,
    o: Option<TermRef<'_>>,
) -> impl Iterator<Item = Quad> {
    g.quads_for_pattern(s, p, o, Some(GraphNameRef::DefaultGraph))
        .filter_map(Result::ok)
}

pub fn objects(g: &Store, s: &Term, p: NamedNodeRef<'_>) -> Vec<Term> {
    match as_subject(s) {
        Some(s) => default_graph(g, Some(s), Some(p), None)
            .map(|q| q.object)
            .collect(),
        None => Vec::new(),
    }
}

pub fn object(g: &Store, s: &Term, p: NamedNodeRef<'_>) -> Option<Term> {
    objects(g, s, p).into_iter().next()
}

pub fn subjects(g: &Store, p: NamedNodeRef<'_>, o: &Term) -> Vec<Term> {
    default_graph(g, None, Some(p), Some(o.as_ref()))
        .map(|q| q.subject.into())
        .collect()
}

pub fn subjects_with(g: &Store, p: NamedNodeRef<'_>) -> Vec<Term> {
    dedup(default_graph(g, None, Some(p), None).map(|q| q.subject.into()))
}

pub fn objects_with(g: &Store, p: NamedNodeRef<'_>) -> Vec<Term> {
    dedup(default_graph(g, None, Some(p), None).map(|q| q.object))
}

pub fn triples_of(g: &Store, s: &Term) -> Vec<Triple> {
    match as_subject(s) {
        Some(s) => default_graph(g, Some(s), None, None)
            .map(Triple::from)
            .collect(),
        None => Vec::new(),
    }
}

pub fn triples(g: &Store) -> Vec<Triple> {
    default_graph(g, None, None, None)
        .map(Triple::from)
        .collect()
}

pub fn contains(g: &Store, t: &Triple) -> bool {
    g.contains(t.as_ref().in_graph(GraphNameRef::DefaultGraph))
        .unwrap_or(false)
}

/// Inserts a triple, returning whether it was new.
pub fn insert(g: &Store, t: &Triple) -> Result<bool> {
    let quad = t.as_ref().in_graph(GraphNameRef::DefaultGraph);
    if g.contains(quad)? {
        return Ok(false);
    }
    g.insert(quad)?;
    Ok(true)
}

pub fn remove(g: &Store, t: &Triple) -> Result<()> {
    g.remove(t.as_ref().in_graph(GraphNameRef::DefaultGraph))?;
    Ok(())
}

pub fn dedup<T: Clone + Eq + std::hash::Hash>(items: impl IntoIterator<Item = T>) -> Vec<T> {
    let mut seen = HashSet::new();
    items
        .into_iter()
        .filter(|i| seen.insert(i.clone()))
        .collect()
}

/// Members of an RDF list.
pub fn list(g: &Store, head: &Term) -> Vec<Term> {
    let mut items = Vec::new();
    let mut seen = HashSet::new();
    let mut current = head.clone();
    while current != Term::from(rdf::NIL) && seen.insert(current.clone()) {
        match object(g, &current, rdf::FIRST) {
            Some(first) => items.push(first),
            None => break,
        }
        match object(g, &current, rdf::REST) {
            Some(rest) => current = rest,
            None => break,
        }
    }
    items
}

pub fn is_true(term: Option<&Term>) -> bool {
    matches!(term, Some(Term::Literal(l)) if l.value() == "true" || l.value() == "1")
}

pub fn integer(term: &Term) -> Option<u64> {
    match term {
        Term::Literal(l) => l.value().parse().ok(),
        _ => None,
    }
}

/// SHACL instance test: `rdf:type/rdfs:subClassOf*` reaches `class`.
pub fn is_instance_of(g: &Store, node: &Term, class: &Term) -> bool {
    let mut stack = objects(g, node, rdf::TYPE);
    let mut seen = HashSet::new();
    while let Some(c) = stack.pop() {
        if &c == class {
            return true;
        }
        if seen.insert(c.clone()) {
            stack.extend(objects(g, &c, rdfs::SUB_CLASS_OF));
        }
    }
    false
}

/// All instances of `class`, including instances of its subclasses.
pub fn instances_of(g: &Store, class: &Term) -> Vec<Term> {
    let mut classes = vec![class.clone()];
    let mut seen: HashSet<Term> = classes.iter().cloned().collect();
    let mut i = 0;
    while i < classes.len() {
        for sub in subjects(g, rdfs::SUB_CLASS_OF, &classes[i]) {
            if seen.insert(sub.clone()) {
                classes.push(sub);
            }
        }
        i += 1;
    }
    dedup(classes.iter().flat_map(|c| subjects(g, rdf::TYPE, c)))
}

pub fn parse_format(name: &str) -> Result<RdfFormat> {
    RdfFormat::from_extension(name)
        .or_else(|| RdfFormat::from_media_type(name))
        .ok_or_else(|| anyhow!("unknown RDF format {name}"))
}

pub fn load(g: &Store, data: &[u8], format: RdfFormat) -> Result<()> {
    g.load_from_slice(RdfParser::from_format(format).without_named_graphs(), data)
        .context("failed to parse RDF")?;
    Ok(())
}

pub fn load_turtle(g: &Store, turtle: &str) -> Result<()> {
    load(g, turtle.as_bytes(), RdfFormat::Turtle)
}

pub fn store_from(triples: impl IntoIterator<Item = Triple>) -> Result<Store> {
    let g = new_store();
    for t in triples {
        insert(&g, &t)?;
    }
    Ok(g)
}

pub fn serialize(triples: impl IntoIterator<Item = Triple>, format: RdfFormat) -> Result<String> {
    let mut serializer = RdfSerializer::from_format(format);
    if matches!(format, RdfFormat::Turtle | RdfFormat::TriG) {
        for (p, ns) in PREFIXES {
            serializer = serializer.with_prefix(*p, *ns)?;
        }
    }
    let mut writer = serializer.for_writer(Vec::new());
    for t in triples {
        writer.serialize_triple(&t)?;
    }
    Ok(String::from_utf8(writer.finish()?)?)
}

pub fn to_turtle(triples: impl IntoIterator<Item = Triple>) -> Result<String> {
    serialize(triples, RdfFormat::Turtle)
}

fn mentions_variable(query: &str, name: &str) -> bool {
    query.match_indices(name).any(|(i, _)| {
        i > 0
            && matches!(query.as_bytes()[i - 1], b'?' | b'$')
            && !query[i + name.len()..]
                .chars()
                .next()
                .is_some_and(|c| c.is_alphanumeric() || c == '_')
    })
}

fn project(pattern: &mut spargebra::algebra::GraphPattern, variable: &Variable) {
    use spargebra::algebra::GraphPattern;
    match pattern {
        GraphPattern::Project { variables, .. } => {
            if !variables.contains(variable) {
                variables.push(variable.clone());
            }
        }
        GraphPattern::Distinct { inner }
        | GraphPattern::Reduced { inner }
        | GraphPattern::Slice { inner, .. } => project(inner, variable),
        _ => {}
    }
}

/// Parses a query and pre-binds variables (SHACL-style `$this` pre-binding).
/// A custom SPARQL function implementation.
pub type CustomFn = Arc<dyn Fn(&[Term]) -> Option<Term> + Send + Sync>;

/// Built-in `qf:` functions available in every SPARQL query.
pub fn builtin_functions() -> Vec<(NamedNode, CustomFn)> {
    vec![
        (
            qf::EPOCH_MILLIS_TO_DATE_TIME.into_owned(),
            Arc::new(|args: &[Term]| {
                let ms: i64 = match args.first()? {
                    Term::Literal(l) => l.value().split('.').next()?.parse().ok()?,
                    _ => return None,
                };
                Some(Literal::new_typed_literal(epoch_millis_to_iso(ms), xsd::DATE_TIME).into())
            }),
        ),
        (
            qf::HASH64.into_owned(),
            Arc::new(|args: &[Term]| {
                let text = lexical(args.first()?)?;
                Some(Literal::from(hash64(text)).into())
            }),
        ),
    ]
}

/// FNV-1a hash truncated to a non-negative `i64`, for deterministic integer ids.
pub fn hash64(text: &str) -> i64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in text.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    (h & i64::MAX as u64) as i64
}

pub fn epoch_millis_to_iso(ms: i64) -> String {
    let (secs, millis) = (ms.div_euclid(1000), ms.rem_euclid(1000));
    let (days, sod) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    let frac = if millis > 0 {
        format!(".{millis:03}")
    } else {
        String::new()
    };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}{frac}Z",
        sod / 3600,
        sod % 3600 / 60,
        sod % 60
    )
}

fn prepare(
    query: &str,
    bindings: &[(&str, Term)],
    functions: &[(NamedNode, CustomFn)],
) -> Result<oxigraph::sparql::PreparedSparqlQuery> {
    let mut parsed = spargebra::SparqlParser::new()
        .parse_query(query)
        .with_context(|| format!("invalid SPARQL query:\n{query}"))?;
    let bindings: Vec<_> = bindings
        .iter()
        .filter(|(name, _)| mentions_variable(query, name))
        .map(|(name, term)| (Variable::new_unchecked(*name), term.clone()))
        .collect();
    if let spargebra::Query::Select { pattern, .. } = &mut parsed {
        for (v, _) in &bindings {
            project(pattern, v);
        }
    }
    let mut evaluator = SparqlEvaluator::new();
    for (name, f) in builtin_functions().iter().chain(functions) {
        let f = f.clone();
        evaluator = evaluator.with_custom_function(name.clone(), move |args| f(args));
    }
    let mut prepared = evaluator.for_query(parsed);
    for (v, term) in bindings {
        prepared = prepared.substitute_variable(v, term);
    }
    Ok(prepared)
}

pub fn select(g: &Store, query: &str, bindings: &[(&str, Term)]) -> Result<Vec<QuerySolution>> {
    select_with(g, query, bindings, &[])
}

pub fn select_with(
    g: &Store,
    query: &str,
    bindings: &[(&str, Term)],
    functions: &[(NamedNode, CustomFn)],
) -> Result<Vec<QuerySolution>> {
    match prepare(query, bindings, functions)?.on_store(g).execute()? {
        QueryResults::Solutions(solutions) => Ok(solutions.collect::<Result<_, _>>()?),
        _ => Err(anyhow!("expected a SELECT query")),
    }
}

pub fn ask(g: &Store, query: &str, bindings: &[(&str, Term)]) -> Result<bool> {
    ask_with(g, query, bindings, &[])
}

pub fn ask_with(
    g: &Store,
    query: &str,
    bindings: &[(&str, Term)],
    functions: &[(NamedNode, CustomFn)],
) -> Result<bool> {
    match prepare(query, bindings, functions)?.on_store(g).execute()? {
        QueryResults::Boolean(b) => Ok(b),
        _ => Err(anyhow!("expected an ASK query")),
    }
}

pub fn construct_with(
    g: &Store,
    query: &str,
    bindings: &[(&str, Term)],
    functions: &[(NamedNode, CustomFn)],
) -> Result<Vec<Triple>> {
    match prepare(query, bindings, functions)?.on_store(g).execute()? {
        QueryResults::Graph(triples) => Ok(triples.collect::<Result<_, _>>()?),
        _ => Err(anyhow!("expected a CONSTRUCT query")),
    }
}

/// Runs any query with built-in functions.
pub fn query(g: &Store, query: &str) -> Result<QueryResults<'static>> {
    Ok(prepare(query, &[], &[])?.on_store(g).execute()?)
}

pub static EMPTY: LazyLock<Store> = LazyLock::new(new_store);

/// Compares two terms with the SPARQL operator `op`; incomparable terms yield `false`.
pub fn sparql_compare(a: &Term, op: &str, b: &Term) -> bool {
    if matches!(a, Term::BlankNode(_)) || matches!(b, Term::BlankNode(_)) {
        return false;
    }
    // SPARQL orders xsd:boolean (false < true) but Oxigraph does not implement it.
    if let (Some(x), Some(y)) = (as_boolean(a), as_boolean(b)) {
        return match op {
            "<" => !x & y,
            "<=" => x <= y,
            ">" => x & !y,
            ">=" => x >= y,
            "=" => x == y,
            _ => x != y,
        };
    }
    ask(&EMPTY, &format!("ASK {{ FILTER({a} {op} {b}) }}"), &[]).unwrap_or(false)
}

fn as_boolean(t: &Term) -> Option<bool> {
    match t {
        Term::Literal(l) if l.datatype() == oxigraph::model::vocab::xsd::BOOLEAN => {
            oxsdatatypes::Boolean::from_str(l.value())
                .ok()
                .map(bool::from)
        }
        _ => None,
    }
}

/// The string value used by SHACL string-based constraints.
pub fn lexical(term: &Term) -> Option<&str> {
    match term {
        Term::NamedNode(n) => Some(n.as_str()),
        Term::Literal(l) => Some(l.value()),
        _ => None,
    }
}

pub fn local_name(iri: &str) -> &str {
    iri.rsplit(['#', '/']).next().unwrap_or(iri)
}
