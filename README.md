# queryrdf

A semantic, in-memory lakehouse metadata catalog, similar to Apache Iceberg and DuckLake catalogs:

- **Model**: catalog metadata (namespaces, tables, schemas, fields, snapshots, data files and column stats) is RDF in the `qc:` vocabulary (`https://w3id.org/queryrdf/catalog#`). Fields can link to ontology concepts with `qc:concept`.
- **In memory**: the live catalog is an [Oxigraph](https://github.com/oxigraph/oxigraph) in-memory store that you query with SPARQL.
- **Persistence**: [redb](https://github.com/cberner/redb) holds a term dictionary, a dictionary-encoded SPO triple table, an append-only commit log and the persisted shapes. Commits are ACID and use optimistic concurrency like Iceberg: each changeset records its base version and fails with a conflict if the catalog has moved on. Any earlier version can be rebuilt from the log (time travel).
- **SHACL**: a built-in SHACL Core + SHACL-SPARQL validator checks every commit against [`shapes/catalog.ttl`](shapes/catalog.ttl) plus any shapes you add. Commits that do not conform are rejected and rolled back.
- **SHACL-AF instead of RML**: sources are first converted ("lifted") to generic structural RDF (JSON → `j:` predicates and blank nodes, CSV → `qs:Column` nodes with inferred types and stats, or any RDF format). `sh:TripleRule` / `sh:SPARQLRule` rules (with `sh:order`, `sh:condition`, node expressions, `sh:SPARQLFunction`s and optional fixpoint iteration) then map that RDF into the catalog model. The same engine runs rules over the catalog itself, for annotation or for export to DCAT/PROV.

## Build

```sh
cargo build --release
cargo test
```

## Usage

```sh
export QUERYRDF_DB=catalog.redb
queryrdf init

# Iceberg table metadata -> catalog (every version upserts schemas/snapshots)
queryrdf ingest examples/iceberg/v1.metadata.json -r rules/iceberg.ttl
queryrdf ingest examples/iceberg/v2.metadata.json -r rules/iceberg.ttl
queryrdf ingest examples/iceberg/broken.metadata.json -r rules/iceberg.ttl   # rejected by SHACL, exit code 2

# CSV -> table + append snapshot + data file + column stats
queryrdf ingest examples/csv/customers.csv -r rules/csv.ttl

# Rules over the catalog: add semantic annotations, export DCAT/PROV
queryrdf transform -r rules/annotate.ttl --commit -m "annotate fields"
queryrdf transform -r rules/dcat.ttl --output ttl

# Query (qc:, qs:, schema:, dcat:, ... prefixes are predeclared), time travel, history
queryrdf query 'SELECT ?table ?field ?concept WHERE {
  ?t qc:name ?table ; qc:currentSchema/qc:field ?f . ?f qc:name ?field ; qc:concept ?concept }'
queryrdf query --at 1 'SELECT (COUNT(*) AS ?n) WHERE { ?s a qc:Schema }'
queryrdf log
queryrdf dump --at 2

# Validation and extra persisted shapes
queryrdf validate --shapes my-shapes.ttl
queryrdf shapes add my-shapes.ttl          # enforced on all later commits
queryrdf lift examples/csv/customers.csv   # show the structural RDF the rules see
```

`ingest --dry-run` prints the mapped triples and the validation report without committing. By default the rules see the source graph merged with the current catalog (so they can chain snapshots to their parent); `--isolated` turns that off.

## Layout

| Path | Contents |
|------|----------|
| `src/vocab.rs` | `qc:`, `qs:`, `qf:` and SHACL vocabulary |
| `src/graph.rs` | Oxigraph helpers, SPARQL with `$this` substitution, custom functions `qf:epochMillisToDateTime`, `qf:hash64` |
| `src/shacl.rs` | SHACL Core + SPARQL constraints, property paths, validation reports |
| `src/rules.rs` | SHACL-AF rules, node expressions, SPARQL functions |
| `src/lift.rs` | JSON / CSV / RDF lifting into structural RDF |
| `src/persist.rs` | redb tables, commit log, optimistic concurrency |
| `src/catalog.rs` | Catalog API: changesets, validated commits, time travel, shapes |
| `shapes/catalog.ttl` | Built-in catalog shapes |
| `rules/` | Mapping rules: `iceberg.ttl`, `csv.ttl`, `annotate.ttl`, `dcat.ttl` |

## Not yet implemented

- Reading Iceberg manifest lists/manifests (Avro) or Parquet footers for file-level stats.
- Running queries over the data files (e.g. via DuckDB).
- SHACL-AF features beyond rules, node expressions and SPARQL functions (e.g. `sh:SPARQLTarget` types, custom constraint components).
