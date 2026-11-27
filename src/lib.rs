//! queryrdf: a semantic, in-memory lakehouse metadata catalog persisted with redb,
//! validated with SHACL and transformed with SHACL Advanced Features rules.

pub mod catalog;
pub mod graph;
pub mod lift;
pub mod persist;
pub mod rules;
pub mod shacl;
pub mod vocab;

pub use catalog::{Catalog, Changeset, CommitError};
pub use rules::{RuleOptions, RuleSet};
pub use shacl::{ShapesGraph, ValidationReport, Validator};
