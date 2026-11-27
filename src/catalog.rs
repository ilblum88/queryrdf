//! The catalog: an in-memory RDF graph (queried with SPARQL), validated with SHACL on every
//! commit and persisted to redb.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use oxigraph::model::*;
use oxigraph::store::Store;

use crate::graph;
use crate::persist::{CommitRecord, Persist, PersistError};
use crate::shacl::{ShapesGraph, ValidationReport, Validator};

/// Built-in SHACL shapes for the catalog model.
pub const CATALOG_SHAPES: &str = include_str!("../shapes/catalog.ttl");

#[derive(Debug, thiserror::Error)]
pub enum CommitError {
    #[error("commit conflict: based on version {base} but the catalog is at version {current}")]
    Conflict { base: u64, current: u64 },
    #[error("commit rejected by SHACL validation\n{0}")]
    Invalid(ValidationReport),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// A set of triple additions/removals based on a catalog version.
#[derive(Debug, Clone, Default)]
pub struct Changeset {
    pub base: u64,
    pub added: Vec<Triple>,
    pub removed: Vec<Triple>,
}

impl Changeset {
    pub fn add(&mut self, t: Triple) -> &mut Self {
        self.added.push(t);
        self
    }

    pub fn remove(&mut self, t: Triple) -> &mut Self {
        self.removed.push(t);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct CommitOutcome {
    pub version: u64,
    pub added: usize,
    pub removed: usize,
}

pub struct Catalog {
    persist: Persist,
    data: Store,
    version: u64,
    shapes: ShapesGraph,
    /// Validate every commit against the shapes (on by default).
    pub validate_commits: bool,
}

impl Catalog {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let persist = Persist::open(path)?;
        let (version, triples) = persist.load()?;
        let data = graph::store_from(triples)?;
        let shapes = load_shapes(&persist, None)?;
        Ok(Self {
            persist,
            data,
            version,
            shapes,
            validate_commits: true,
        })
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    /// The live in-memory catalog graph.
    pub fn store(&self) -> &Store {
        &self.data
    }

    pub fn shapes(&self) -> &ShapesGraph {
        &self.shapes
    }

    pub fn changeset(&self) -> Changeset {
        Changeset {
            base: self.version,
            ..Default::default()
        }
    }

    /// A changeset that makes the catalog describe `triples` for every (subject, predicate)
    /// pair they mention, replacing previous values of those pairs.
    pub fn upsert(&self, triples: impl IntoIterator<Item = Triple>) -> Changeset {
        let new: Vec<Triple> = graph::dedup(triples);
        let new_set: HashSet<&Triple> = new.iter().collect();
        let pairs: HashSet<(Term, NamedNode)> = new
            .iter()
            .map(|t| (t.subject.clone().into(), t.predicate.clone()))
            .collect();
        let mut cs = self.changeset();
        for (s, p) in &pairs {
            for o in graph::objects(&self.data, s, p.as_ref()) {
                let t = Triple::new(graph::as_subject(s).unwrap().into_owned(), p.clone(), o);
                if !new_set.contains(&t) {
                    cs.removed.push(t);
                }
            }
        }
        cs.added = new
            .into_iter()
            .filter(|t| !graph::contains(&self.data, t))
            .collect();
        cs
    }

    pub fn validate(&self) -> Result<ValidationReport> {
        Validator::new(&self.shapes, &self.data).validate()
    }

    /// Validates the catalog as it would be after applying `cs`, without committing.
    pub fn preview(&self, cs: &Changeset) -> Result<ValidationReport> {
        let (added, removed) = self.apply(cs)?;
        let report = self.validate();
        self.revert(&added, &removed)?;
        report
    }

    /// Applies a changeset: optimistic concurrency check, SHACL validation, then a durable
    /// redb commit. On any failure the in-memory graph is left unchanged.
    pub fn commit(&mut self, cs: Changeset, message: &str) -> Result<CommitOutcome, CommitError> {
        if cs.base != self.version {
            return Err(CommitError::Conflict {
                base: cs.base,
                current: self.version,
            });
        }
        let (added, removed) = self.apply(&cs)?;
        if added.is_empty() && removed.is_empty() {
            return Ok(CommitOutcome {
                version: self.version,
                added: 0,
                removed: 0,
            });
        }
        if self.validate_commits {
            let report = match self.validate() {
                Ok(r) => r,
                Err(e) => {
                    self.revert(&added, &removed)?;
                    return Err(e.into());
                }
            };
            if !report.conforms() {
                self.revert(&added, &removed)?;
                return Err(CommitError::Invalid(report));
            }
        }
        match self.persist.commit(self.version, message, &added, &removed) {
            Ok(record) => {
                self.version = record.version;
                Ok(CommitOutcome {
                    version: record.version,
                    added: added.len(),
                    removed: removed.len(),
                })
            }
            Err(e) => {
                self.revert(&added, &removed)?;
                Err(match e {
                    PersistError::Conflict { base, current } => {
                        CommitError::Conflict { base, current }
                    }
                    PersistError::Other(e) => CommitError::Other(e),
                })
            }
        }
    }

    fn apply(&self, cs: &Changeset) -> Result<(Vec<Triple>, Vec<Triple>)> {
        let mut removed = Vec::new();
        for t in graph::dedup(cs.removed.iter().cloned()) {
            if graph::contains(&self.data, &t) {
                graph::remove(&self.data, &t)?;
                removed.push(t);
            }
        }
        let mut added = Vec::new();
        for t in &cs.added {
            if graph::insert(&self.data, t)? {
                added.push(t.clone());
            }
        }
        Ok((added, removed))
    }

    fn revert(&self, added: &[Triple], removed: &[Triple]) -> Result<()> {
        for t in added {
            graph::remove(&self.data, t)?;
        }
        for t in removed {
            graph::insert(&self.data, t)?;
        }
        Ok(())
    }

    /// Replays commits made by other processes since this catalog was loaded.
    pub fn refresh(&mut self) -> Result<u64> {
        let commits = self.persist.commits_after(self.version)?;
        for c in &commits {
            for t in c.removed_triples()? {
                graph::remove(&self.data, &t)?;
            }
            for t in c.added_triples()? {
                graph::insert(&self.data, &t)?;
            }
            self.version = c.version;
        }
        Ok(commits.len() as u64)
    }

    /// Reconstructs the catalog graph as of `version` by replaying the commit log.
    pub fn at_version(&self, version: u64) -> Result<Store> {
        if version > self.version {
            anyhow::bail!(
                "version {version} does not exist (latest is {})",
                self.version
            );
        }
        let store = graph::new_store();
        for c in self
            .persist
            .commits_after(0)?
            .into_iter()
            .take_while(|c| c.version <= version)
        {
            for t in c.removed_triples()? {
                graph::remove(&store, &t)?;
            }
            for t in c.added_triples()? {
                graph::insert(&store, &t)?;
            }
        }
        Ok(store)
    }

    pub fn log(&self) -> Result<Vec<CommitRecord>> {
        self.persist.commits_after(0)
    }

    /// Persists an additional shapes graph after checking the current catalog conforms to it.
    pub fn add_shapes(
        &mut self,
        name: &str,
        turtle: &str,
        force: bool,
    ) -> Result<ValidationReport> {
        // Shapes files may extend existing shapes, so check the merged shapes graph.
        let merged = load_shapes(&self.persist, Some((name, turtle)))?;
        let report = Validator::new(&merged, &self.data).validate()?;
        if report.conforms() || force {
            self.persist.put_shapes(name, turtle)?;
            self.shapes = merged;
        }
        Ok(report)
    }

    pub fn remove_shapes(&mut self, name: &str) -> Result<bool> {
        let removed = self.persist.remove_shapes(name)?;
        self.shapes = load_shapes(&self.persist, None)?;
        Ok(removed)
    }

    /// Additional shapes graphs persisted in the catalog, as (name, Turtle).
    pub fn persisted_shapes(&self) -> Result<Vec<(String, String)>> {
        self.persist.shapes()
    }

    pub fn shape_sets(&self) -> Result<Vec<String>> {
        Ok(self.persist.shapes()?.into_iter().map(|(n, _)| n).collect())
    }
}

/// Builtin plus persisted shapes; `replacement` overrides (or adds) one persisted shapes set.
fn load_shapes(persist: &Persist, replacement: Option<(&str, &str)>) -> Result<ShapesGraph> {
    let g = graph::new_store();
    graph::load_turtle(&g, CATALOG_SHAPES)?;
    for (name, turtle) in persist.shapes()? {
        if replacement.is_none_or(|(n, _)| n != name) {
            graph::load_turtle(&g, &turtle)?;
        }
    }
    if let Some((name, turtle)) = replacement {
        graph::load_turtle(&g, turtle).with_context(|| format!("in shapes '{name}'"))?;
    }
    ShapesGraph::from_store(g)
}
