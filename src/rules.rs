//! SHACL Advanced Features rules (`sh:TripleRule`, `sh:SPARQLRule`, node expressions and
//! `sh:SPARQLFunction`), used for model transformation instead of RML.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use oxigraph::model::*;
use oxigraph::store::Store;

use crate::graph::{self, CustomFn, dedup, is_true, list, object, objects};
use crate::shacl::{Path, ShapesGraph, Validator, prefix_declarations};
use crate::vocab::{rdf, sh};

/// A SHACL-AF node expression.
#[derive(Debug, Clone)]
pub enum NodeExpr {
    This,
    Constant(Term),
    Path {
        path: Path,
        nodes: Option<Box<NodeExpr>>,
    },
    Filter {
        shape: Term,
        nodes: Box<NodeExpr>,
    },
    Intersection(Vec<NodeExpr>),
    Union(Vec<NodeExpr>),
    Function {
        function: NamedNode,
        args: Vec<NodeExpr>,
    },
}

#[derive(Debug, Clone)]
pub enum RuleKind {
    Triple {
        subject: NodeExpr,
        predicate: NodeExpr,
        object: NodeExpr,
    },
    Sparql {
        construct: String,
    },
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub id: Term,
    pub shape: Term,
    pub order: f64,
    pub conditions: Vec<Term>,
    pub kind: RuleKind,
}

#[derive(Debug, Clone)]
struct SparqlFunction {
    params: Vec<(String, bool)>,
    query: String,
    is_ask: bool,
}

impl SparqlFunction {
    fn call(&self, g: &Store, args: &[Term]) -> Result<Option<Term>> {
        let mut bindings = Vec::new();
        for (i, (name, optional)) in self.params.iter().enumerate() {
            match args.get(i) {
                Some(a) => bindings.push((name.as_str(), a.clone())),
                None if *optional => {}
                None => return Ok(None),
            }
        }
        if self.is_ask {
            return Ok(Some(
                Literal::from(graph::ask(g, &self.query, &bindings)?).into(),
            ));
        }
        Ok(graph::select(g, &self.query, &bindings)?
            .into_iter()
            .next()
            .and_then(|s| s.values().first().cloned().flatten()))
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RuleOptions {
    /// Re-run all rules until no new triples are inferred.
    pub iterate: bool,
    pub max_iterations: usize,
}

impl Default for RuleOptions {
    fn default() -> Self {
        Self {
            iterate: false,
            max_iterations: 32,
        }
    }
}

#[derive(Debug, Default)]
pub struct Inference {
    /// Every triple produced by a rule (deduplicated), whether or not it already existed.
    pub produced: Vec<Triple>,
    /// Triples that were not in the data graph before execution.
    pub inferred: Vec<Triple>,
    pub iterations: usize,
}

/// A parsed SHACL-AF rules graph.
pub struct RuleSet {
    shapes: ShapesGraph,
    rules: Vec<Rule>,
    functions: HashMap<NamedNode, SparqlFunction>,
}

impl RuleSet {
    pub fn from_turtle(turtle: &str) -> Result<Self> {
        let g = graph::new_store();
        graph::load_turtle(&g, turtle)?;
        Self::from_store(g)
    }

    pub fn from_store(g: Store) -> Result<Self> {
        let functions = parse_functions(&g)?;
        let mut rules = Vec::new();
        for shape in graph::subjects_with(&g, sh::RULE) {
            if is_true(object(&g, &shape, sh::DEACTIVATED).as_ref()) {
                continue;
            }
            for id in objects(&g, &shape, sh::RULE) {
                if is_true(object(&g, &id, sh::DEACTIVATED).as_ref()) {
                    continue;
                }
                let kind = parse_rule_kind(&g, &id, &functions)
                    .with_context(|| format!("invalid rule {id}"))?;
                let order = object(&g, &id, sh::ORDER)
                    .and_then(|o| graph::lexical(&o)?.parse().ok())
                    .unwrap_or(0.0);
                rules.push(Rule {
                    conditions: objects(&g, &id, sh::CONDITION),
                    shape: shape.clone(),
                    id,
                    order,
                    kind,
                });
            }
        }
        rules.sort_by(|a, b| {
            a.order
                .total_cmp(&b.order)
                .then_with(|| a.id.to_string().cmp(&b.id.to_string()))
        });
        Ok(Self {
            shapes: ShapesGraph::from_store(g)?,
            rules,
            functions,
        })
    }

    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    /// The rules graph's `sh:SPARQLFunction`s, exposed as custom SPARQL functions.
    pub fn sparql_functions(&self) -> Vec<(NamedNode, CustomFn)> {
        self.functions
            .iter()
            .map(|(name, f)| {
                let f = f.clone();
                let custom: CustomFn =
                    Arc::new(move |args: &[Term]| f.call(&graph::EMPTY, args).ok().flatten());
                (name.clone(), custom)
            })
            .collect()
    }

    /// Executes the rules, adding inferred triples to `data`.
    pub fn execute(&self, data: &Store, options: RuleOptions) -> Result<Inference> {
        let functions = self.sparql_functions();
        let mut result = Inference::default();
        let mut produced_seen = HashSet::new();
        loop {
            result.iterations += 1;
            let mut new_triples = 0;
            for rule in &self.rules {
                let Some(shape) = self.shapes.shape(&rule.shape) else {
                    continue;
                };
                let produced = {
                    let validator = Validator::new(&self.shapes, data);
                    let mut produced = Vec::new();
                    'focus: for focus in validator.focus_nodes(shape)? {
                        for condition in &rule.conditions {
                            if !validator.conforms(condition, &focus)? {
                                continue 'focus;
                            }
                        }
                        self.fire(rule, &focus, data, &validator, &functions, &mut produced)
                            .with_context(|| format!("rule {} failed on {focus}", rule.id))?;
                    }
                    produced
                };
                for t in produced {
                    if graph::insert(data, &t)? {
                        new_triples += 1;
                        result.inferred.push(t.clone());
                    }
                    if produced_seen.insert(t.clone()) {
                        result.produced.push(t);
                    }
                }
            }
            if !options.iterate || new_triples == 0 || result.iterations >= options.max_iterations {
                break;
            }
        }
        Ok(result)
    }

    fn fire(
        &self,
        rule: &Rule,
        focus: &Term,
        data: &Store,
        validator: &Validator<'_>,
        functions: &[(NamedNode, CustomFn)],
        out: &mut Vec<Triple>,
    ) -> Result<()> {
        match &rule.kind {
            RuleKind::Sparql { construct } => {
                out.extend(graph::construct_with(
                    data,
                    construct,
                    &[("this", focus.clone())],
                    functions,
                )?);
            }
            RuleKind::Triple {
                subject,
                predicate,
                object,
            } => {
                let subjects = self.eval(subject, focus, data, validator)?;
                let predicates = self.eval(predicate, focus, data, validator)?;
                let objects = self.eval(object, focus, data, validator)?;
                for s in &subjects {
                    let s: NamedOrBlankNode = match s {
                        Term::NamedNode(n) => n.clone().into(),
                        Term::BlankNode(b) => b.clone().into(),
                        _ => continue,
                    };
                    for p in &predicates {
                        let Term::NamedNode(p) = p else { continue };
                        for o in &objects {
                            out.push(Triple::new(s.clone(), p.clone(), o.clone()));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Evaluates a node expression for a focus node.
    pub fn eval(
        &self,
        expr: &NodeExpr,
        focus: &Term,
        data: &Store,
        validator: &Validator<'_>,
    ) -> Result<Vec<Term>> {
        Ok(match expr {
            NodeExpr::This => vec![focus.clone()],
            NodeExpr::Constant(t) => vec![t.clone()],
            NodeExpr::Path { path, nodes } => {
                let starts = match nodes {
                    Some(n) => self.eval(n, focus, data, validator)?,
                    None => vec![focus.clone()],
                };
                dedup(starts.iter().flat_map(|s| path.eval(data, s)))
            }
            NodeExpr::Filter { shape, nodes } => {
                let mut kept = Vec::new();
                for n in self.eval(nodes, focus, data, validator)? {
                    if validator.conforms(shape, &n)? {
                        kept.push(n);
                    }
                }
                kept
            }
            NodeExpr::Intersection(parts) => {
                let mut iter = parts.iter();
                let mut acc = match iter.next() {
                    Some(first) => self.eval(first, focus, data, validator)?,
                    None => Vec::new(),
                };
                for part in iter {
                    let other: HashSet<_> = self
                        .eval(part, focus, data, validator)?
                        .into_iter()
                        .collect();
                    acc.retain(|t| other.contains(t));
                }
                acc
            }
            NodeExpr::Union(parts) => {
                let mut acc = Vec::new();
                for part in parts {
                    acc.extend(self.eval(part, focus, data, validator)?);
                }
                dedup(acc)
            }
            NodeExpr::Function { function, args } => {
                let f = self
                    .functions
                    .get(function)
                    .ok_or_else(|| anyhow!("unknown function {function}"))?;
                let mut combos: Vec<Vec<Term>> = vec![Vec::new()];
                for arg in args {
                    let values = self.eval(arg, focus, data, validator)?;
                    combos = combos
                        .iter()
                        .flat_map(|c| {
                            values.iter().map(move |v| {
                                let mut c = c.clone();
                                c.push(v.clone());
                                c
                            })
                        })
                        .collect();
                }
                let mut values = Vec::new();
                for combo in combos {
                    if let Some(v) = f.call(data, &combo)? {
                        values.push(v);
                    }
                }
                dedup(values)
            }
        })
    }
}

fn parse_functions(g: &Store) -> Result<HashMap<NamedNode, SparqlFunction>> {
    let mut functions = HashMap::new();
    for f in graph::instances_of(g, &sh::SPARQL_FUNCTION.into()) {
        let Term::NamedNode(name) = &f else {
            bail!("sh:SPARQLFunction must be an IRI: {f}");
        };
        let mut params: Vec<(f64, String, bool)> = Vec::new();
        for (i, p) in objects(g, &f, sh::PARAMETER).into_iter().enumerate() {
            let Some(Term::NamedNode(path)) = object(g, &p, sh::PATH) else {
                bail!("parameter of {name} needs an IRI sh:path");
            };
            let order = object(g, &p, sh::ORDER)
                .and_then(|o| graph::lexical(&o)?.parse().ok())
                .unwrap_or(i as f64);
            params.push((
                order,
                graph::local_name(path.as_str()).to_owned(),
                is_true(object(g, &p, sh::OPTIONAL).as_ref()),
            ));
        }
        params.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        let prefixes = prefix_declarations(g, &f);
        let (body, is_ask) = match (object(g, &f, sh::SELECT), object(g, &f, sh::ASK)) {
            (Some(s), _) => (s, false),
            (None, Some(a)) => (a, true),
            _ => bail!("sh:SPARQLFunction {name} needs sh:select or sh:ask"),
        };
        functions.insert(
            name.clone(),
            SparqlFunction {
                params: params.into_iter().map(|(_, n, o)| (n, o)).collect(),
                query: format!("{prefixes}{}", graph::lexical(&body).unwrap_or_default()),
                is_ask,
            },
        );
    }
    Ok(functions)
}

fn parse_rule_kind(
    g: &Store,
    id: &Term,
    functions: &HashMap<NamedNode, SparqlFunction>,
) -> Result<RuleKind> {
    if let Some(construct) = object(g, id, sh::CONSTRUCT) {
        let text = graph::lexical(&construct).ok_or_else(|| anyhow!("invalid sh:construct"))?;
        return Ok(RuleKind::Sparql {
            construct: format!("{}{text}", prefix_declarations(g, id)),
        });
    }
    let part = |p: NamedNodeRef<'_>| -> Result<NodeExpr> {
        let t = object(g, id, p).ok_or_else(|| anyhow!("sh:TripleRule requires {p}"))?;
        parse_node_expr(g, &t, functions)
    };
    if graph::is_instance_of(g, id, &sh::TRIPLE_RULE.into()) || object(g, id, sh::SUBJECT).is_some()
    {
        return Ok(RuleKind::Triple {
            subject: part(sh::SUBJECT)?,
            predicate: part(sh::PREDICATE)?,
            object: part(sh::OBJECT)?,
        });
    }
    bail!("unsupported rule type (expected sh:TripleRule or sh:SPARQLRule)")
}

fn parse_node_expr(
    g: &Store,
    t: &Term,
    functions: &HashMap<NamedNode, SparqlFunction>,
) -> Result<NodeExpr> {
    if t == &Term::from(sh::THIS) {
        return Ok(NodeExpr::This);
    }
    if !matches!(t, Term::BlankNode(_)) {
        return Ok(NodeExpr::Constant(t.clone()));
    }
    let nested = |p: NamedNodeRef<'_>| -> Result<Option<Box<NodeExpr>>> {
        object(g, t, p)
            .map(|n| parse_node_expr(g, &n, functions).map(Box::new))
            .transpose()
    };
    if let Some(path) = object(g, t, sh::PATH) {
        return Ok(NodeExpr::Path {
            path: Path::parse(g, &path)?,
            nodes: nested(sh::NODES)?,
        });
    }
    if let Some(shape) = object(g, t, sh::FILTER_SHAPE) {
        let nodes = nested(sh::NODES)?.unwrap_or(Box::new(NodeExpr::This));
        return Ok(NodeExpr::Filter { shape, nodes });
    }
    let members = |head: &Term| -> Result<Vec<NodeExpr>> {
        list(g, head)
            .iter()
            .map(|m| parse_node_expr(g, m, functions))
            .collect()
    };
    if let Some(head) = object(g, t, sh::INTERSECTION) {
        return Ok(NodeExpr::Intersection(members(&head)?));
    }
    if let Some(head) = object(g, t, sh::UNION) {
        return Ok(NodeExpr::Union(members(&head)?));
    }
    for triple in graph::triples_of(g, t) {
        if functions.contains_key(&triple.predicate) {
            let args = if triple.object == Term::from(rdf::NIL) {
                Vec::new()
            } else {
                members(&triple.object)?
            };
            return Ok(NodeExpr::Function {
                function: triple.predicate,
                args,
            });
        }
    }
    bail!("unsupported node expression {t}")
}
