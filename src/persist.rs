//! redb persistence: a dictionary-encoded triple table holding the current catalog state and an
//! append-only commit log of deltas used for optimistic concurrency and time travel.

use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result};
use oxigraph::model::{Term, Triple};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

const TERMS: TableDefinition<u64, &str> = TableDefinition::new("terms");
const TERM_IDS: TableDefinition<&str, u64> = TableDefinition::new("term_ids");
const SPO: TableDefinition<(u64, u64, u64), ()> = TableDefinition::new("spo");
const COMMITS: TableDefinition<u64, &str> = TableDefinition::new("commits");
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
const SHAPES: TableDefinition<&str, &str> = TableDefinition::new("shapes");

const VERSION: &str = "version";
const NEXT_TERM: &str = "next_term_id";

/// One entry in the catalog commit log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitRecord {
    pub version: u64,
    pub parent: u64,
    pub timestamp_ms: i64,
    pub message: String,
    /// Added triples in N-Triples syntax.
    pub added: Vec<String>,
    /// Removed triples in N-Triples syntax.
    pub removed: Vec<String>,
}

impl CommitRecord {
    pub fn added_triples(&self) -> Result<Vec<Triple>> {
        parse_all(&self.added)
    }

    pub fn removed_triples(&self) -> Result<Vec<Triple>> {
        parse_all(&self.removed)
    }
}

fn parse_all(lines: &[String]) -> Result<Vec<Triple>> {
    lines
        .iter()
        .map(|l| Triple::from_str(l).with_context(|| format!("corrupt triple in commit log: {l}")))
        .collect()
}

#[derive(Debug, thiserror::Error)]
pub enum PersistError {
    #[error("commit conflict: based on version {base} but the catalog is at version {current}")]
    Conflict { base: u64, current: u64 },
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub struct Persist {
    db: Database,
}

fn meta(table: &impl ReadableTable<&'static str, u64>, key: &str) -> Result<u64> {
    Ok(table.get(key)?.map(|v| v.value()).unwrap_or(0))
}

impl Persist {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let db = Database::create(path.as_ref())
            .with_context(|| format!("cannot open redb database {}", path.as_ref().display()))?;
        let txn = db.begin_write()?;
        {
            txn.open_table(TERMS)?;
            txn.open_table(TERM_IDS)?;
            txn.open_table(SPO)?;
            txn.open_table(COMMITS)?;
            txn.open_table(META)?;
            txn.open_table(SHAPES)?;
        }
        txn.commit()?;
        Ok(Self { db })
    }

    pub fn version(&self) -> Result<u64> {
        let txn = self.db.begin_read()?;
        meta(&txn.open_table(META)?, VERSION)
    }

    /// Current catalog state and its version, read from one consistent snapshot.
    pub fn load(&self) -> Result<(u64, Vec<Triple>)> {
        let txn = self.db.begin_read()?;
        let version = meta(&txn.open_table(META)?, VERSION)?;
        let terms = txn.open_table(TERMS)?;
        let decode = |id: u64| -> Result<Term> {
            let text = terms.get(id)?.context("dangling term id")?;
            Ok(Term::from_str(text.value())?)
        };
        let mut triples = Vec::new();
        for entry in txn.open_table(SPO)?.iter()? {
            let (s, p, o) = entry?.0.value();
            let (Term::NamedNode(p), s) = (decode(p)?, decode(s)?) else {
                anyhow::bail!("corrupt predicate");
            };
            let s: oxigraph::model::NamedOrBlankNode = match s {
                Term::NamedNode(n) => n.into(),
                Term::BlankNode(b) => b.into(),
                _ => anyhow::bail!("corrupt subject"),
            };
            triples.push(Triple::new(s, p, decode(o)?));
        }
        Ok((version, triples))
    }

    /// Atomically applies a delta if the stored version still equals `base`.
    pub fn commit(
        &self,
        base: u64,
        message: &str,
        added: &[Triple],
        removed: &[Triple],
    ) -> Result<CommitRecord, PersistError> {
        let inner = || -> Result<Result<CommitRecord, PersistError>> {
            let txn = self.db.begin_write()?;
            let record;
            {
                let mut meta_table = txn.open_table(META)?;
                let current = meta(&meta_table, VERSION)?;
                if current != base {
                    return Ok(Err(PersistError::Conflict { base, current }));
                }
                let mut next_term = meta(&meta_table, NEXT_TERM)?.max(1);
                let mut terms = txn.open_table(TERMS)?;
                let mut ids = txn.open_table(TERM_IDS)?;
                let mut spo = txn.open_table(SPO)?;
                let mut encode = |term: Term, create: bool| -> Result<Option<u64>> {
                    let text = term.to_string();
                    if let Some(id) = ids.get(text.as_str())? {
                        return Ok(Some(id.value()));
                    }
                    if !create {
                        return Ok(None);
                    }
                    let id = next_term;
                    next_term += 1;
                    ids.insert(text.as_str(), id)?;
                    terms.insert(id, text.as_str())?;
                    Ok(Some(id))
                };
                for t in removed {
                    let key = (
                        encode(t.subject.clone().into(), false)?,
                        encode(t.predicate.clone().into(), false)?,
                        encode(t.object.clone(), false)?,
                    );
                    if let (Some(s), Some(p), Some(o)) = key {
                        spo.remove((s, p, o))?;
                    }
                }
                for t in added {
                    let s = encode(t.subject.clone().into(), true)?.unwrap_or_default();
                    let p = encode(t.predicate.clone().into(), true)?.unwrap_or_default();
                    let o = encode(t.object.clone(), true)?.unwrap_or_default();
                    spo.insert((s, p, o), ())?;
                }
                record = CommitRecord {
                    version: base + 1,
                    parent: base,
                    timestamp_ms: now_ms(),
                    message: message.to_owned(),
                    added: added.iter().map(ToString::to_string).collect(),
                    removed: removed.iter().map(ToString::to_string).collect(),
                };
                txn.open_table(COMMITS)?
                    .insert(record.version, serde_json::to_string(&record)?.as_str())?;
                meta_table.insert(VERSION, record.version)?;
                meta_table.insert(NEXT_TERM, next_term)?;
            }
            txn.commit()?;
            Ok(Ok(record))
        };
        inner()?
    }

    /// Commit records with `version > after`, oldest first.
    pub fn commits_after(&self, after: u64) -> Result<Vec<CommitRecord>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(COMMITS)?;
        let mut out = Vec::new();
        for entry in table.range(after + 1..)? {
            out.push(serde_json::from_str(entry?.1.value())?);
        }
        Ok(out)
    }

    pub fn shapes(&self) -> Result<Vec<(String, String)>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(SHAPES)?;
        let mut out = Vec::new();
        for entry in table.iter()? {
            let (k, v) = entry?;
            out.push((k.value().to_owned(), v.value().to_owned()));
        }
        Ok(out)
    }

    pub fn put_shapes(&self, name: &str, turtle: &str) -> Result<()> {
        let txn = self.db.begin_write()?;
        txn.open_table(SHAPES)?.insert(name, turtle)?;
        txn.commit()?;
        Ok(())
    }

    pub fn remove_shapes(&self, name: &str) -> Result<bool> {
        let txn = self.db.begin_write()?;
        let removed = txn.open_table(SHAPES)?.remove(name)?.is_some();
        txn.commit()?;
        Ok(removed)
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}
