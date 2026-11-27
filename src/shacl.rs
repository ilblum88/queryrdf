//! SHACL Core + SHACL-SPARQL validation over in-memory oxigraph graphs.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::str::FromStr;

use anyhow::{Context, Result, anyhow, bail};
use oxigraph::model::*;
use oxigraph::store::Store;
use regex::Regex;

use crate::graph::{self, dedup, is_true, list, object, objects};
use crate::vocab::{PREFIXES, rdf, rdfs, sh, xsd};

/// A SHACL property path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Path {
    Predicate(NamedNode),
    Inverse(Box<Path>),
    Sequence(Vec<Path>),
    Alternative(Vec<Path>),
    ZeroOrMore(Box<Path>),
    OneOrMore(Box<Path>),
    ZeroOrOne(Box<Path>),
}

impl Path {
    pub fn parse(g: &Store, term: &Term) -> Result<Self> {
        if let Term::NamedNode(n) = term {
            return Ok(Self::Predicate(n.clone()));
        }
        if object(g, term, rdf::FIRST).is_some() {
            let parts = list(g, term)
                .iter()
                .map(|t| Self::parse(g, t))
                .collect::<Result<Vec<_>>>()?;
            return Ok(Self::Sequence(parts));
        }
        if let Some(p) = object(g, term, sh::INVERSE_PATH) {
            return Ok(Self::Inverse(Box::new(Self::parse(g, &p)?)));
        }
        if let Some(p) = object(g, term, sh::ALTERNATIVE_PATH) {
            let parts = list(g, &p)
                .iter()
                .map(|t| Self::parse(g, t))
                .collect::<Result<Vec<_>>>()?;
            return Ok(Self::Alternative(parts));
        }
        if let Some(p) = object(g, term, sh::ZERO_OR_MORE_PATH) {
            return Ok(Self::ZeroOrMore(Box::new(Self::parse(g, &p)?)));
        }
        if let Some(p) = object(g, term, sh::ONE_OR_MORE_PATH) {
            return Ok(Self::OneOrMore(Box::new(Self::parse(g, &p)?)));
        }
        if let Some(p) = object(g, term, sh::ZERO_OR_ONE_PATH) {
            return Ok(Self::ZeroOrOne(Box::new(Self::parse(g, &p)?)));
        }
        bail!("invalid SHACL path {term}")
    }

    fn inverse(&self) -> Path {
        match self {
            Self::Predicate(_) => Self::Inverse(Box::new(self.clone())),
            Self::Inverse(p) => (**p).clone(),
            Self::Sequence(ps) => Self::Sequence(ps.iter().rev().map(Path::inverse).collect()),
            Self::Alternative(ps) => Self::Alternative(ps.iter().map(Path::inverse).collect()),
            Self::ZeroOrMore(p) => Self::ZeroOrMore(Box::new(p.inverse())),
            Self::OneOrMore(p) => Self::OneOrMore(Box::new(p.inverse())),
            Self::ZeroOrOne(p) => Self::ZeroOrOne(Box::new(p.inverse())),
        }
    }

    /// Nodes reachable from `focus` through this path.
    pub fn eval(&self, g: &Store, focus: &Term) -> Vec<Term> {
        match self {
            Self::Predicate(p) => objects(g, focus, p.as_ref()),
            Self::Inverse(inner) => match &**inner {
                Self::Predicate(p) => graph::subjects(g, p.as_ref(), focus),
                other => other.inverse().eval(g, focus),
            },
            Self::Sequence(parts) => {
                let mut current = vec![focus.clone()];
                for part in parts {
                    current = dedup(current.iter().flat_map(|n| part.eval(g, n)));
                }
                current
            }
            Self::Alternative(parts) => dedup(parts.iter().flat_map(|p| p.eval(g, focus))),
            Self::ZeroOrMore(p) => closure(g, p, focus, true),
            Self::OneOrMore(p) => closure(g, p, focus, false),
            Self::ZeroOrOne(p) => dedup(std::iter::once(focus.clone()).chain(p.eval(g, focus))),
        }
    }

    /// SPARQL 1.1 property path syntax.
    pub fn to_sparql(&self) -> String {
        let join =
            |ps: &[Path], sep: &str| ps.iter().map(Path::to_sparql).collect::<Vec<_>>().join(sep);
        match self {
            Self::Predicate(p) => p.to_string(),
            Self::Inverse(p) => format!("^({})", p.to_sparql()),
            Self::Sequence(ps) => format!("({})", join(ps, "/")),
            Self::Alternative(ps) => format!("({})", join(ps, "|")),
            Self::ZeroOrMore(p) => format!("({})*", p.to_sparql()),
            Self::OneOrMore(p) => format!("({})+", p.to_sparql()),
            Self::ZeroOrOne(p) => format!("({})?", p.to_sparql()),
        }
    }
}

fn closure(g: &Store, path: &Path, start: &Term, include_start: bool) -> Vec<Term> {
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    let mut stack = if include_start {
        vec![start.clone()]
    } else {
        path.eval(g, start)
    };
    while let Some(n) = stack.pop() {
        if seen.insert(n.clone()) {
            stack.extend(path.eval(g, &n));
            result.push(n);
        }
    }
    result
}

/// How focus nodes are selected for a shape.
#[derive(Debug, Clone)]
pub enum Target {
    Class(Term),
    Node(Term),
    SubjectsOf(NamedNode),
    ObjectsOf(NamedNode),
    Sparql(String),
}

#[derive(Debug, Clone)]
pub enum Constraint {
    Class(Term),
    Datatype(NamedNode),
    NodeKind(NamedNode),
    MinCount(u64),
    MaxCount(u64),
    MinExclusive(Term),
    MinInclusive(Term),
    MaxExclusive(Term),
    MaxInclusive(Term),
    MinLength(u64),
    MaxLength(u64),
    Pattern(Regex),
    LanguageIn(Vec<String>),
    UniqueLang,
    Equals(NamedNode),
    Disjoint(NamedNode),
    LessThan(NamedNode),
    LessThanOrEquals(NamedNode),
    Not(Term),
    And(Vec<Term>),
    Or(Vec<Term>),
    Xone(Vec<Term>),
    Node(Term),
    Property(Term),
    QualifiedMinCount {
        shape: Term,
        min: u64,
        siblings: Vec<Term>,
    },
    QualifiedMaxCount {
        shape: Term,
        max: u64,
        siblings: Vec<Term>,
    },
    Closed {
        allowed: HashSet<NamedNode>,
    },
    HasValue(Term),
    In(Vec<Term>),
    Sparql {
        node: Term,
        query: String,
        messages: Vec<Term>,
    },
}

impl Constraint {
    pub fn component(&self) -> NamedNode {
        let name = match self {
            Self::Class(_) => "Class",
            Self::Datatype(_) => "Datatype",
            Self::NodeKind(_) => "NodeKind",
            Self::MinCount(_) => "MinCount",
            Self::MaxCount(_) => "MaxCount",
            Self::MinExclusive(_) => "MinExclusive",
            Self::MinInclusive(_) => "MinInclusive",
            Self::MaxExclusive(_) => "MaxExclusive",
            Self::MaxInclusive(_) => "MaxInclusive",
            Self::MinLength(_) => "MinLength",
            Self::MaxLength(_) => "MaxLength",
            Self::Pattern(_) => "Pattern",
            Self::LanguageIn(_) => "LanguageIn",
            Self::UniqueLang => "UniqueLang",
            Self::Equals(_) => "Equals",
            Self::Disjoint(_) => "Disjoint",
            Self::LessThan(_) => "LessThan",
            Self::LessThanOrEquals(_) => "LessThanOrEquals",
            Self::Not(_) => "Not",
            Self::And(_) => "And",
            Self::Or(_) => "Or",
            Self::Xone(_) => "Xone",
            Self::Node(_) => "Node",
            Self::Property(_) => "Property",
            Self::QualifiedMinCount { .. } => "QualifiedMinCount",
            Self::QualifiedMaxCount { .. } => "QualifiedMaxCount",
            Self::Closed { .. } => "Closed",
            Self::HasValue(_) => "HasValue",
            Self::In(_) => "In",
            Self::Sparql { .. } => "SPARQL",
        };
        NamedNode::new_unchecked(format!("{}{name}ConstraintComponent", sh::NS))
    }
}

#[derive(Debug, Clone)]
pub struct Shape {
    pub id: Term,
    pub path: Option<(Path, Term)>,
    pub targets: Vec<Target>,
    pub constraints: Vec<Constraint>,
    pub severity: NamedNode,
    pub messages: Vec<Term>,
    pub deactivated: bool,
}

/// Parsed shapes graph.
pub struct ShapesGraph {
    pub graph: Store,
    shapes: HashMap<Term, Shape>,
    order: Vec<Term>,
}

/// `PREFIX` declarations from the `sh:prefixes` of `node`.
pub fn prefix_declarations(g: &Store, node: &Term) -> String {
    let mut out = String::new();
    for decls in objects(g, node, sh::PREFIXES) {
        for decl in objects(g, &decls, sh::DECLARE) {
            if let (Some(p), Some(ns)) = (
                object(g, &decl, sh::PREFIX),
                object(g, &decl, sh::NAMESPACE),
            ) && let (Some(p), Some(ns)) = (graph::lexical(&p), graph::lexical(&ns))
            {
                out.push_str(&format!("PREFIX {p}: <{ns}>\n"));
            }
        }
    }
    out
}

fn sparql_text(g: &Store, node: &Term, predicate: NamedNodeRef<'_>) -> Option<String> {
    let body = object(g, node, predicate)?;
    Some(format!(
        "{}{}",
        prefix_declarations(g, node),
        graph::lexical(&body)?
    ))
}

impl ShapesGraph {
    pub fn from_turtle(turtle: &str) -> Result<Self> {
        let g = graph::new_store();
        graph::load_turtle(&g, turtle)?;
        Self::from_store(g)
    }

    pub fn from_store(graph: Store) -> Result<Self> {
        let g = &graph;
        let mut queue: Vec<Term> = Vec::new();
        queue.extend(graph::instances_of(g, &sh::NODE_SHAPE.into()));
        queue.extend(graph::instances_of(g, &sh::PROPERTY_SHAPE.into()));
        for p in [
            sh::TARGET_CLASS,
            sh::TARGET_NODE,
            sh::TARGET_SUBJECTS_OF,
            sh::TARGET_OBJECTS_OF,
            sh::TARGET,
            sh::RULE,
        ] {
            queue.extend(graph::subjects_with(g, p));
        }
        for p in [sh::CONDITION, sh::FILTER_SHAPE] {
            queue.extend(graph::objects_with(g, p));
        }
        let mut shapes = HashMap::new();
        let mut order = Vec::new();
        while let Some(id) = queue.pop() {
            if shapes.contains_key(&id) {
                continue;
            }
            let shape = parse_shape(g, &id).with_context(|| format!("invalid shape {id}"))?;
            for c in &shape.constraints {
                match c {
                    Constraint::Not(s) | Constraint::Node(s) | Constraint::Property(s) => {
                        queue.push(s.clone())
                    }
                    Constraint::And(ss) | Constraint::Or(ss) | Constraint::Xone(ss) => {
                        queue.extend(ss.iter().cloned())
                    }
                    Constraint::QualifiedMinCount {
                        shape, siblings, ..
                    }
                    | Constraint::QualifiedMaxCount {
                        shape, siblings, ..
                    } => {
                        queue.push(shape.clone());
                        queue.extend(siblings.iter().cloned());
                    }
                    _ => {}
                }
            }
            order.push(id.clone());
            shapes.insert(id, shape);
        }
        order.sort_by_key(|t| t.to_string());
        Ok(Self {
            graph,
            shapes,
            order,
        })
    }

    pub fn shape(&self, id: &Term) -> Option<&Shape> {
        self.shapes.get(id)
    }

    pub fn shapes(&self) -> impl Iterator<Item = &Shape> {
        self.order.iter().filter_map(|id| self.shapes.get(id))
    }

    pub fn len(&self) -> usize {
        self.shapes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.shapes.is_empty()
    }
}

fn named(t: &Term, what: &str) -> Result<NamedNode> {
    match t {
        Term::NamedNode(n) => Ok(n.clone()),
        _ => Err(anyhow!("{what} must be an IRI, found {t}")),
    }
}

fn int_param(t: &Term, what: &str) -> Result<u64> {
    graph::integer(t).ok_or_else(|| anyhow!("{what} must be a non-negative integer, found {t}"))
}

fn parse_shape(g: &Store, id: &Term) -> Result<Shape> {
    let path = match object(g, id, sh::PATH) {
        Some(p) => Some((Path::parse(g, &p)?, p)),
        None => None,
    };
    let mut targets = Vec::new();
    for c in objects(g, id, sh::TARGET_CLASS) {
        targets.push(Target::Class(c));
    }
    if graph::is_instance_of(g, id, &rdfs::CLASS.into())
        && (graph::is_instance_of(g, id, &sh::NODE_SHAPE.into())
            || graph::is_instance_of(g, id, &sh::PROPERTY_SHAPE.into()))
    {
        targets.push(Target::Class(id.clone()));
    }
    for n in objects(g, id, sh::TARGET_NODE) {
        targets.push(Target::Node(n));
    }
    for p in objects(g, id, sh::TARGET_SUBJECTS_OF) {
        targets.push(Target::SubjectsOf(named(&p, "sh:targetSubjectsOf")?));
    }
    for p in objects(g, id, sh::TARGET_OBJECTS_OF) {
        targets.push(Target::ObjectsOf(named(&p, "sh:targetObjectsOf")?));
    }
    for t in objects(g, id, sh::TARGET) {
        let query = sparql_text(g, &t, sh::SELECT)
            .ok_or_else(|| anyhow!("only SPARQL-based sh:target with sh:select is supported"))?;
        targets.push(Target::Sparql(query));
    }

    let mut constraints = Vec::new();
    let one = |p: NamedNodeRef<'_>| objects(g, id, p);
    for v in one(sh::CLASS) {
        constraints.push(Constraint::Class(v));
    }
    for v in one(sh::DATATYPE) {
        constraints.push(Constraint::Datatype(named(&v, "sh:datatype")?));
    }
    for v in one(sh::NODE_KIND) {
        constraints.push(Constraint::NodeKind(named(&v, "sh:nodeKind")?));
    }
    for v in one(sh::MIN_COUNT) {
        constraints.push(Constraint::MinCount(int_param(&v, "sh:minCount")?));
    }
    for v in one(sh::MAX_COUNT) {
        constraints.push(Constraint::MaxCount(int_param(&v, "sh:maxCount")?));
    }
    for v in one(sh::MIN_EXCLUSIVE) {
        constraints.push(Constraint::MinExclusive(v));
    }
    for v in one(sh::MIN_INCLUSIVE) {
        constraints.push(Constraint::MinInclusive(v));
    }
    for v in one(sh::MAX_EXCLUSIVE) {
        constraints.push(Constraint::MaxExclusive(v));
    }
    for v in one(sh::MAX_INCLUSIVE) {
        constraints.push(Constraint::MaxInclusive(v));
    }
    for v in one(sh::MIN_LENGTH) {
        constraints.push(Constraint::MinLength(int_param(&v, "sh:minLength")?));
    }
    for v in one(sh::MAX_LENGTH) {
        constraints.push(Constraint::MaxLength(int_param(&v, "sh:maxLength")?));
    }
    for v in one(sh::PATTERN) {
        let pattern = graph::lexical(&v).ok_or_else(|| anyhow!("invalid sh:pattern"))?;
        let flags = object(g, id, sh::FLAGS)
            .and_then(|f| graph::lexical(&f).map(str::to_owned))
            .unwrap_or_default();
        let flags: String = flags.chars().filter(|c| "imsx".contains(*c)).collect();
        let re = if flags.is_empty() {
            Regex::new(pattern)
        } else {
            Regex::new(&format!("(?{flags}){pattern}"))
        }
        .with_context(|| format!("invalid sh:pattern {pattern}"))?;
        constraints.push(Constraint::Pattern(re));
    }
    for v in one(sh::LANGUAGE_IN) {
        let langs = list(g, &v)
            .iter()
            .filter_map(|t| graph::lexical(t).map(str::to_lowercase))
            .collect();
        constraints.push(Constraint::LanguageIn(langs));
    }
    if is_true(object(g, id, sh::UNIQUE_LANG).as_ref()) {
        constraints.push(Constraint::UniqueLang);
    }
    for v in one(sh::EQUALS) {
        constraints.push(Constraint::Equals(named(&v, "sh:equals")?));
    }
    for v in one(sh::DISJOINT) {
        constraints.push(Constraint::Disjoint(named(&v, "sh:disjoint")?));
    }
    for v in one(sh::LESS_THAN) {
        constraints.push(Constraint::LessThan(named(&v, "sh:lessThan")?));
    }
    for v in one(sh::LESS_THAN_OR_EQUALS) {
        constraints.push(Constraint::LessThanOrEquals(named(
            &v,
            "sh:lessThanOrEquals",
        )?));
    }
    for v in one(sh::NOT) {
        constraints.push(Constraint::Not(v));
    }
    for v in one(sh::AND) {
        constraints.push(Constraint::And(list(g, &v)));
    }
    for v in one(sh::OR) {
        constraints.push(Constraint::Or(list(g, &v)));
    }
    for v in one(sh::XONE) {
        constraints.push(Constraint::Xone(list(g, &v)));
    }
    for v in one(sh::NODE) {
        constraints.push(Constraint::Node(v));
    }
    for v in one(sh::PROPERTY) {
        constraints.push(Constraint::Property(v));
    }
    for shape in one(sh::QUALIFIED_VALUE_SHAPE) {
        let siblings = if is_true(object(g, id, sh::QUALIFIED_VALUE_SHAPES_DISJOINT).as_ref()) {
            let mut s = Vec::new();
            for parent in graph::subjects(g, sh::PROPERTY, id) {
                for sibling in objects(g, &parent, sh::PROPERTY) {
                    if &sibling != id {
                        s.extend(objects(g, &sibling, sh::QUALIFIED_VALUE_SHAPE));
                    }
                }
            }
            s
        } else {
            Vec::new()
        };
        if let Some(min) = object(g, id, sh::QUALIFIED_MIN_COUNT) {
            constraints.push(Constraint::QualifiedMinCount {
                shape: shape.clone(),
                min: int_param(&min, "sh:qualifiedMinCount")?,
                siblings: siblings.clone(),
            });
        }
        if let Some(max) = object(g, id, sh::QUALIFIED_MAX_COUNT) {
            constraints.push(Constraint::QualifiedMaxCount {
                shape: shape.clone(),
                max: int_param(&max, "sh:qualifiedMaxCount")?,
                siblings,
            });
        }
    }
    if is_true(object(g, id, sh::CLOSED).as_ref()) {
        let mut allowed = HashSet::new();
        for p in objects(g, id, sh::PROPERTY) {
            if let Some(Term::NamedNode(n)) = object(g, &p, sh::PATH) {
                allowed.insert(n);
            }
        }
        for ignored in objects(g, id, sh::IGNORED_PROPERTIES) {
            for p in list(g, &ignored) {
                allowed.insert(named(&p, "sh:ignoredProperties member")?);
            }
        }
        constraints.push(Constraint::Closed { allowed });
    }
    for v in one(sh::HAS_VALUE) {
        constraints.push(Constraint::HasValue(v));
    }
    for v in one(sh::IN) {
        constraints.push(Constraint::In(list(g, &v)));
    }
    for node in one(sh::SPARQL) {
        if is_true(object(g, &node, sh::DEACTIVATED).as_ref()) {
            continue;
        }
        let query = sparql_text(g, &node, sh::SELECT)
            .ok_or_else(|| anyhow!("sh:sparql constraint {node} requires sh:select"))?;
        constraints.push(Constraint::Sparql {
            messages: objects(g, &node, sh::MESSAGE),
            node,
            query,
        });
    }

    let severity = match object(g, id, sh::SEVERITY) {
        Some(Term::NamedNode(n)) => n,
        _ => sh::VIOLATION.into_owned(),
    };
    Ok(Shape {
        id: id.clone(),
        path,
        targets,
        constraints,
        severity,
        messages: objects(g, id, sh::MESSAGE),
        deactivated: is_true(object(g, id, sh::DEACTIVATED).as_ref()),
    })
}

#[derive(Debug, Clone)]
pub struct ValidationResult {
    pub focus: Term,
    pub path: Option<Term>,
    pub value: Option<Term>,
    pub shape: Term,
    pub component: NamedNode,
    pub severity: NamedNode,
    pub messages: Vec<Term>,
    pub constraint: Option<Term>,
}

#[derive(Debug, Clone, Default)]
pub struct ValidationReport {
    pub results: Vec<ValidationResult>,
    /// Shapes-graph triples describing blank-node result paths.
    path_triples: Vec<Triple>,
}

impl ValidationReport {
    pub fn conforms(&self) -> bool {
        self.results.is_empty()
    }

    pub fn violations(&self) -> usize {
        self.results
            .iter()
            .filter(|r| r.severity.as_ref() == sh::VIOLATION)
            .count()
    }

    pub fn to_triples(&self) -> Vec<Triple> {
        let mut out = self.path_triples.clone();
        let report: NamedOrBlankNode = BlankNode::default().into();
        out.push(Triple::new(
            report.clone(),
            rdf::TYPE,
            sh::VALIDATION_REPORT,
        ));
        out.push(Triple::new(
            report.clone(),
            sh::CONFORMS,
            Literal::from(self.conforms()),
        ));
        for r in &self.results {
            let node: NamedOrBlankNode = BlankNode::default().into();
            out.push(Triple::new(report.clone(), sh::RESULT, node.clone()));
            out.push(Triple::new(node.clone(), rdf::TYPE, sh::VALIDATION_RESULT));
            out.push(Triple::new(node.clone(), sh::FOCUS_NODE, r.focus.clone()));
            if let Some(p) = &r.path {
                out.push(Triple::new(node.clone(), sh::RESULT_PATH, p.clone()));
            }
            if let Some(v) = &r.value {
                out.push(Triple::new(node.clone(), sh::VALUE, v.clone()));
            }
            out.push(Triple::new(node.clone(), sh::SOURCE_SHAPE, r.shape.clone()));
            out.push(Triple::new(
                node.clone(),
                sh::SOURCE_CONSTRAINT_COMPONENT,
                r.component.clone(),
            ));
            if let Some(c) = &r.constraint {
                out.push(Triple::new(node.clone(), sh::SOURCE_CONSTRAINT, c.clone()));
            }
            out.push(Triple::new(
                node.clone(),
                sh::RESULT_SEVERITY,
                r.severity.clone(),
            ));
            for m in &r.messages {
                out.push(Triple::new(node.clone(), sh::RESULT_MESSAGE, m.clone()));
            }
        }
        out
    }
}

pub fn compact(term: &Term) -> String {
    if let Term::NamedNode(n) = term {
        for (prefix, ns) in PREFIXES {
            if let Some(local) = n.as_str().strip_prefix(ns) {
                return format!("{prefix}:{local}");
            }
        }
    }
    term.to_string()
}

impl fmt::Display for ValidationResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let component = compact(&self.component.clone().into());
        let component = component
            .trim_start_matches("sh:")
            .trim_end_matches("ConstraintComponent");
        write!(
            f,
            "[{}] {} focus={}",
            graph::local_name(self.severity.as_str()),
            component,
            compact(&self.focus)
        )?;
        if let Some(p) = &self.path {
            write!(f, " path={}", compact(p))?;
        }
        if let Some(v) = &self.value {
            write!(f, " value={}", compact(v))?;
        }
        write!(f, " shape={}", compact(&self.shape))?;
        for m in &self.messages {
            write!(f, " — {}", graph::lexical(m).unwrap_or_default())?;
        }
        Ok(())
    }
}

impl fmt::Display for ValidationReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.conforms() {
            return writeln!(f, "conforms: true");
        }
        writeln!(f, "conforms: false ({} results)", self.results.len())?;
        for r in &self.results {
            writeln!(f, "  {r}")?;
        }
        Ok(())
    }
}

const MAX_DEPTH: usize = 48;

pub struct Validator<'a> {
    shapes: &'a ShapesGraph,
    data: &'a Store,
}

impl<'a> Validator<'a> {
    pub fn new(shapes: &'a ShapesGraph, data: &'a Store) -> Self {
        Self { shapes, data }
    }

    pub fn validate(&self) -> Result<ValidationReport> {
        let mut results = Vec::new();
        for shape in self.shapes.shapes() {
            if shape.deactivated {
                continue;
            }
            for focus in self.focus_nodes(shape)? {
                self.validate_shape(shape, &focus, &mut results, 0)?;
            }
        }
        let mut path_triples = Vec::new();
        let mut copied = HashSet::new();
        for r in &results {
            if let Some(p @ Term::BlankNode(_)) = &r.path {
                copy_bnode_closure(&self.shapes.graph, p, &mut copied, &mut path_triples);
            }
        }
        Ok(ValidationReport {
            results,
            path_triples,
        })
    }

    pub fn focus_nodes(&self, shape: &Shape) -> Result<Vec<Term>> {
        let mut nodes = Vec::new();
        for target in &shape.targets {
            match target {
                Target::Class(c) => nodes.extend(graph::instances_of(self.data, c)),
                Target::Node(n) => nodes.push(n.clone()),
                Target::SubjectsOf(p) => nodes.extend(graph::subjects_with(self.data, p.as_ref())),
                Target::ObjectsOf(p) => nodes.extend(graph::objects_with(self.data, p.as_ref())),
                Target::Sparql(q) => {
                    for s in graph::select(self.data, q, &[])? {
                        if let Some(t) = s.get("this") {
                            nodes.push(t.clone());
                        }
                    }
                }
            }
        }
        Ok(dedup(nodes))
    }

    /// Whether `node` conforms to the shape `shape_id` (unknown shapes always conform).
    pub fn conforms(&self, shape_id: &Term, node: &Term) -> Result<bool> {
        self.conforms_at(shape_id, node, 0)
    }

    fn conforms_at(&self, shape_id: &Term, node: &Term, depth: usize) -> Result<bool> {
        let Some(shape) = self.shapes.shape(shape_id) else {
            return Ok(true);
        };
        if depth > MAX_DEPTH {
            return Ok(true);
        }
        let mut results = Vec::new();
        self.validate_shape(shape, node, &mut results, depth + 1)?;
        Ok(results.is_empty())
    }

    fn validate_shape(
        &self,
        shape: &Shape,
        focus: &Term,
        out: &mut Vec<ValidationResult>,
        depth: usize,
    ) -> Result<()> {
        if shape.deactivated {
            return Ok(());
        }
        let values = match &shape.path {
            Some((path, _)) => path.eval(self.data, focus),
            None => vec![focus.clone()],
        };
        for constraint in &shape.constraints {
            self.check(shape, constraint, focus, &values, out, depth)?;
        }
        Ok(())
    }

    fn check(
        &self,
        shape: &Shape,
        c: &Constraint,
        focus: &Term,
        values: &[Term],
        out: &mut Vec<ValidationResult>,
        depth: usize,
    ) -> Result<()> {
        let data = self.data;
        let result = |value: Option<Term>| ValidationResult {
            focus: focus.clone(),
            path: shape.path.as_ref().map(|(_, t)| t.clone()),
            value,
            shape: shape.id.clone(),
            component: c.component(),
            severity: shape.severity.clone(),
            messages: shape.messages.clone(),
            constraint: None,
        };
        let mut each = |failed: &dyn Fn(&Term) -> Result<bool>| -> Result<()> {
            for v in values {
                if failed(v)? {
                    out.push(result(Some(v.clone())));
                }
            }
            Ok(())
        };
        match c {
            Constraint::Class(class) => each(&|v| Ok(!graph::is_instance_of(data, v, class)))?,
            Constraint::Datatype(dt) => each(&|v| {
                Ok(!matches!(v, Term::Literal(l) if l.datatype() == dt.as_ref() && well_formed(l)))
            })?,
            Constraint::NodeKind(kind) => each(&|v| Ok(!node_kind_matches(v, kind.as_ref())))?,
            Constraint::MinCount(n) => {
                if (values.len() as u64) < *n {
                    out.push(result(None));
                }
            }
            Constraint::MaxCount(n) => {
                if (values.len() as u64) > *n {
                    out.push(result(None));
                }
            }
            Constraint::MinExclusive(b) => each(&|v| Ok(!graph::sparql_compare(v, ">", b)))?,
            Constraint::MinInclusive(b) => each(&|v| Ok(!graph::sparql_compare(v, ">=", b)))?,
            Constraint::MaxExclusive(b) => each(&|v| Ok(!graph::sparql_compare(v, "<", b)))?,
            Constraint::MaxInclusive(b) => each(&|v| Ok(!graph::sparql_compare(v, "<=", b)))?,
            Constraint::MinLength(n) => {
                each(&|v| Ok(graph::lexical(v).is_none_or(|s| (s.chars().count() as u64) < *n)))?
            }
            Constraint::MaxLength(n) => {
                each(&|v| Ok(graph::lexical(v).is_none_or(|s| (s.chars().count() as u64) > *n)))?
            }
            Constraint::Pattern(re) => {
                each(&|v| Ok(graph::lexical(v).is_none_or(|s| !re.is_match(s))))?
            }
            Constraint::LanguageIn(langs) => each(&|v| {
                Ok(
                    !matches!(v, Term::Literal(l) if l.language().is_some_and(|tag| {
                        let tag = tag.to_lowercase();
                        langs.iter().any(|r| tag == *r || tag.starts_with(&format!("{r}-")))
                    })),
                )
            })?,
            Constraint::UniqueLang => {
                let mut counts: HashMap<String, usize> = HashMap::new();
                for v in values {
                    if let Term::Literal(l) = v
                        && let Some(lang) = l.language()
                    {
                        *counts.entry(lang.to_lowercase()).or_default() += 1;
                    }
                }
                for _ in counts.values().filter(|n| **n > 1) {
                    out.push(result(None));
                }
            }
            Constraint::Equals(p) => {
                let others = objects(data, focus, p.as_ref());
                for v in values.iter().filter(|v| !others.contains(v)) {
                    out.push(result(Some(v.clone())));
                }
                for o in others.iter().filter(|o| !values.contains(o)) {
                    out.push(result(Some(o.clone())));
                }
            }
            Constraint::Disjoint(p) => {
                let others = objects(data, focus, p.as_ref());
                each(&|v| Ok(others.contains(v)))?
            }
            Constraint::LessThan(p) | Constraint::LessThanOrEquals(p) => {
                let op = if matches!(c, Constraint::LessThan(_)) {
                    "<"
                } else {
                    "<="
                };
                let others = objects(data, focus, p.as_ref());
                for v in values {
                    for o in &others {
                        if !graph::sparql_compare(v, op, o) {
                            out.push(result(Some(v.clone())));
                        }
                    }
                }
            }
            Constraint::Not(s) => each(&|v| self.conforms_at(s, v, depth))?,
            Constraint::And(ss) => each(&|v| {
                for s in ss {
                    if !self.conforms_at(s, v, depth)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            })?,
            Constraint::Or(ss) => each(&|v| {
                for s in ss {
                    if self.conforms_at(s, v, depth)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            })?,
            Constraint::Xone(ss) => each(&|v| {
                let mut n = 0;
                for s in ss {
                    if self.conforms_at(s, v, depth)? {
                        n += 1;
                    }
                }
                Ok(n != 1)
            })?,
            Constraint::Node(s) => each(&|v| Ok(!self.conforms_at(s, v, depth)?))?,
            Constraint::Property(s) => {
                if let Some(property) = self.shapes.shape(s)
                    && depth <= MAX_DEPTH
                {
                    for v in values {
                        self.validate_shape(property, v, out, depth + 1)?;
                    }
                }
            }
            Constraint::QualifiedMinCount {
                shape: qs,
                siblings,
                ..
            }
            | Constraint::QualifiedMaxCount {
                shape: qs,
                siblings,
                ..
            } => {
                let mut count = 0u64;
                'values: for v in values {
                    if !self.conforms_at(qs, v, depth)? {
                        continue;
                    }
                    for sibling in siblings {
                        if self.conforms_at(sibling, v, depth)? {
                            continue 'values;
                        }
                    }
                    count += 1;
                }
                let failed = match c {
                    Constraint::QualifiedMinCount { min, .. } => count < *min,
                    Constraint::QualifiedMaxCount { max, .. } => count > *max,
                    _ => unreachable!(),
                };
                if failed {
                    out.push(result(None));
                }
            }
            Constraint::Closed { allowed } => {
                for v in values {
                    for t in graph::triples_of(data, v) {
                        if !allowed.contains(&t.predicate) {
                            out.push(ValidationResult {
                                focus: v.clone(),
                                path: Some(t.predicate.into()),
                                value: Some(t.object),
                                ..result(None)
                            });
                        }
                    }
                }
            }
            Constraint::HasValue(expected) => {
                if !values.contains(expected) {
                    out.push(result(None));
                }
            }
            Constraint::In(allowed) => each(&|v| Ok(!allowed.contains(v)))?,
            Constraint::Sparql {
                node,
                query,
                messages,
            } => {
                let query = match &shape.path {
                    Some((path, _)) => query.replace("$PATH", &path.to_sparql()),
                    None => query.clone(),
                };
                let solutions = graph::select(
                    data,
                    &query,
                    &[("this", focus.clone()), ("currentShape", shape.id.clone())],
                )
                .with_context(|| format!("SPARQL constraint {node} on shape {}", shape.id))?;
                for s in solutions {
                    if matches!(s.get("failure"), Some(Term::Literal(l)) if l.value() == "true") {
                        bail!("SPARQL constraint {node} reported a failure");
                    }
                    let value = s
                        .get("value")
                        .cloned()
                        .or_else(|| shape.path.is_none().then(|| focus.clone()));
                    let path = s
                        .get("path")
                        .cloned()
                        .or_else(|| shape.path.as_ref().map(|(_, t)| t.clone()));
                    let templates = if messages.is_empty() {
                        &shape.messages
                    } else {
                        messages
                    };
                    let messages = templates
                        .iter()
                        .map(|m| substitute_message(m, &s))
                        .collect();
                    out.push(ValidationResult {
                        path,
                        value,
                        messages,
                        constraint: Some(node.clone()),
                        ..result(None)
                    });
                }
            }
        }
        Ok(())
    }
}

fn substitute_message(template: &Term, solution: &oxigraph::sparql::QuerySolution) -> Term {
    let Term::Literal(l) = template else {
        return template.clone();
    };
    let mut text = l.value().to_owned();
    for (var, value) in solution.iter() {
        let rendered = match value {
            Term::Literal(l) => l.value().to_owned(),
            other => compact(other),
        };
        for marker in [
            format!("{{?{}}}", var.as_str()),
            format!("{{${}}}", var.as_str()),
        ] {
            text = text.replace(&marker, &rendered);
        }
    }
    match l.language() {
        Some(lang) => Literal::new_language_tagged_literal_unchecked(text, lang).into(),
        None => Literal::new_simple_literal(text).into(),
    }
}

fn copy_bnode_closure(g: &Store, node: &Term, seen: &mut HashSet<Term>, out: &mut Vec<Triple>) {
    if !matches!(node, Term::BlankNode(_)) || !seen.insert(node.clone()) {
        return;
    }
    for t in graph::triples_of(g, node) {
        copy_bnode_closure(g, &t.object, seen, out);
        out.push(t);
    }
}

fn node_kind_matches(v: &Term, kind: NamedNodeRef<'_>) -> bool {
    let (iri, blank, literal) = (
        matches!(v, Term::NamedNode(_)),
        matches!(v, Term::BlankNode(_)),
        matches!(v, Term::Literal(_)),
    );
    match kind {
        k if k == sh::IRI => iri,
        k if k == sh::BLANK_NODE => blank,
        k if k == sh::LITERAL => literal,
        k if k == sh::BLANK_NODE_OR_IRI => blank || iri,
        k if k == sh::BLANK_NODE_OR_LITERAL => blank || literal,
        k if k == sh::IRI_OR_LITERAL => iri || literal,
        _ => false,
    }
}

/// Lexical-form validity for common XSD datatypes.
pub fn well_formed(l: &Literal) -> bool {
    use oxsdatatypes::*;
    let v = l.value();
    let dt = l.datatype();
    let int = || -> Option<i128> {
        let digits = v.strip_prefix(['+', '-']).unwrap_or(v);
        (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
            .then(|| v.parse::<i128>().ok())
            .flatten()
    };
    let in_range = |lo: i128, hi: i128| int().is_some_and(|i| (lo..=hi).contains(&i));
    match dt {
        d if d == xsd::INTEGER => int().is_some(),
        d if d == xsd::LONG => in_range(i64::MIN.into(), i64::MAX.into()),
        d if d == xsd::INT => in_range(i32::MIN.into(), i32::MAX.into()),
        d if d == xsd::SHORT => in_range(i16::MIN.into(), i16::MAX.into()),
        d if d == xsd::BYTE => in_range(i8::MIN.into(), i8::MAX.into()),
        d if d == xsd::NON_NEGATIVE_INTEGER => int().is_some_and(|i| i >= 0),
        d if d == xsd::POSITIVE_INTEGER => int().is_some_and(|i| i > 0),
        d if d == xsd::NON_POSITIVE_INTEGER => int().is_some_and(|i| i <= 0),
        d if d == xsd::NEGATIVE_INTEGER => int().is_some_and(|i| i < 0),
        d if d == xsd::UNSIGNED_LONG => in_range(0, u64::MAX.into()),
        d if d == xsd::UNSIGNED_INT => in_range(0, u32::MAX.into()),
        d if d == xsd::UNSIGNED_SHORT => in_range(0, u16::MAX.into()),
        d if d == xsd::UNSIGNED_BYTE => in_range(0, u8::MAX.into()),
        d if d == xsd::DECIMAL => Decimal::from_str(v).is_ok(),
        d if d == xsd::DOUBLE => Double::from_str(v).is_ok(),
        d if d == xsd::FLOAT => Float::from_str(v).is_ok(),
        d if d == xsd::BOOLEAN => Boolean::from_str(v).is_ok(),
        d if d == xsd::DATE_TIME => DateTime::from_str(v).is_ok(),
        d if d == xsd::DATE_TIME_STAMP => {
            DateTime::from_str(v).is_ok_and(|d| d.timezone().is_some())
        }
        d if d == xsd::DATE => Date::from_str(v).is_ok(),
        d if d == xsd::TIME => Time::from_str(v).is_ok(),
        d if d == xsd::DURATION => Duration::from_str(v).is_ok(),
        d if d == xsd::DAY_TIME_DURATION => DayTimeDuration::from_str(v).is_ok(),
        d if d == xsd::YEAR_MONTH_DURATION => YearMonthDuration::from_str(v).is_ok(),
        d if d == xsd::G_YEAR => GYear::from_str(v).is_ok(),
        d if d == rdf::LANG_STRING => l.language().is_some(),
        _ => true,
    }
}
