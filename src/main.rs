use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use oxigraph::io::RdfFormat;
use oxigraph::model::Term;
use oxigraph::sparql::QueryResults;
use oxigraph::store::Store;

use queryrdf::catalog::{Catalog, CommitError};
use queryrdf::lift::{self, LiftOptions, SourceFormat};
use queryrdf::shacl::{ShapesGraph, Validator, compact};
use queryrdf::{RuleOptions, RuleSet, graph};

#[derive(Parser)]
#[command(
    version,
    about = "Semantic lakehouse metadata catalog: in-memory RDF + redb + SHACL / SHACL-AF"
)]
struct Cli {
    /// Catalog database file (redb).
    #[arg(
        long,
        global = true,
        default_value = "catalog.redb",
        env = "QUERYRDF_DB"
    )]
    db: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create (or open) a catalog database.
    Init,
    /// Lift a source file, transform it with SHACL-AF rules and commit the result.
    Ingest {
        file: PathBuf,
        /// SHACL-AF rules files.
        #[arg(long, short, required = true)]
        rules: Vec<PathBuf>,
        /// Source format: json, csv or an RDF format (ttl, nt, ...). Defaults to the extension.
        #[arg(long)]
        format: Option<String>,
        #[arg(long, short)]
        message: Option<String>,
        /// Only add triples instead of replacing values of touched (subject, predicate) pairs.
        #[arg(long)]
        append: bool,
        /// Do not expose the current catalog to the rules.
        #[arg(long)]
        isolated: bool,
        /// Run rules to a fixpoint.
        #[arg(long)]
        iterate: bool,
        /// CSV rows to lift as qs:Row nodes.
        #[arg(long, default_value_t = 0)]
        rows: usize,
        /// Print the result and its validation report without committing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Commit RDF triples directly.
    Load {
        file: PathBuf,
        #[arg(long, short)]
        message: Option<String>,
        /// Remove these triples instead of adding them.
        #[arg(long)]
        delete: bool,
    },
    /// Run SHACL-AF rules over the catalog (or an input file) and print or commit the inferences.
    Transform {
        #[arg(long, short, required = true)]
        rules: Vec<PathBuf>,
        /// Transform this source file instead of the catalog.
        #[arg(long)]
        input: Option<PathBuf>,
        #[arg(long)]
        format: Option<String>,
        /// Catalog version to transform.
        #[arg(long)]
        at: Option<u64>,
        /// Commit the inferred triples to the catalog.
        #[arg(long)]
        commit: bool,
        #[arg(long, short)]
        message: Option<String>,
        #[arg(long)]
        iterate: bool,
        /// Output RDF format.
        #[arg(long, default_value = "ttl")]
        output: String,
    },
    /// Print the structural RDF lifted from a JSON/CSV source.
    Lift {
        file: PathBuf,
        #[arg(long)]
        format: Option<String>,
        #[arg(long, default_value_t = 0)]
        rows: usize,
    },
    /// Validate the catalog (or an RDF file) against the catalog shapes and extra shapes.
    Validate {
        #[arg(long)]
        shapes: Vec<PathBuf>,
        #[arg(long)]
        at: Option<u64>,
        /// Validate this RDF file instead of the catalog.
        #[arg(long)]
        data: Option<PathBuf>,
        /// Print the report as Turtle.
        #[arg(long)]
        turtle: bool,
    },
    /// Run a SPARQL query (use @file to read it from a file, - for stdin).
    Query {
        query: String,
        #[arg(long)]
        at: Option<u64>,
    },
    /// Show the commit log.
    Log,
    /// Dump the catalog graph.
    Dump {
        #[arg(long)]
        at: Option<u64>,
        #[arg(long, default_value = "ttl")]
        output: String,
    },
    /// Manage persisted additional shapes.
    Shapes {
        #[command(subcommand)]
        command: ShapesCommand,
    },
}

#[derive(Subcommand)]
enum ShapesCommand {
    Add {
        file: PathBuf,
        #[arg(long)]
        name: Option<String>,
        /// Persist even if the current catalog violates the shapes.
        #[arg(long)]
        force: bool,
    },
    Remove {
        name: String,
    },
    List,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn read(path: &std::path::Path) -> Result<String> {
    std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))
}

fn load_rules(paths: &[PathBuf]) -> Result<RuleSet> {
    let g = graph::new_store();
    for p in paths {
        graph::load_turtle(&g, &read(p)?).with_context(|| format!("in {}", p.display()))?;
    }
    RuleSet::from_store(g)
}

fn copy_into(target: &Store, source: &Store) -> Result<()> {
    for t in graph::triples(source) {
        graph::insert(target, &t)?;
    }
    Ok(())
}

fn source_graph(file: &std::path::Path, format: Option<&str>, rows: usize) -> Result<Store> {
    lift::lift_file(
        file,
        SourceFormat::detect(file, format)?,
        LiftOptions { rows },
    )
}

fn report_commit(
    result: Result<queryrdf::catalog::CommitOutcome, CommitError>,
) -> Result<ExitCode> {
    match result {
        Ok(o) => {
            println!(
                "committed version {} (+{} -{})",
                o.version, o.added, o.removed
            );
            Ok(ExitCode::SUCCESS)
        }
        Err(CommitError::Invalid(report)) => {
            eprintln!("commit rejected by SHACL validation\n{report}");
            Ok(ExitCode::from(2))
        }
        Err(e) => Err(e.into()),
    }
}

fn snapshot(catalog: &Catalog, at: Option<u64>) -> Result<Store> {
    match at {
        Some(v) => catalog.at_version(v),
        None => {
            let s = graph::new_store();
            copy_into(&s, catalog.store())?;
            Ok(s)
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    let mut catalog = Catalog::open(&cli.db)?;
    match cli.command {
        Command::Init => {
            println!(
                "catalog {} at version {}",
                cli.db.display(),
                catalog.version()
            );
        }
        Command::Ingest {
            file,
            rules,
            format,
            message,
            append,
            isolated,
            iterate,
            rows,
            dry_run,
        } => {
            let rules = load_rules(&rules)?;
            let source = source_graph(&file, format.as_deref(), rows)?;
            let source_nodes: std::collections::HashSet<Term> = graph::triples(&source)
                .into_iter()
                .flat_map(|t| [t.subject.into(), t.object])
                .filter(|t: &Term| matches!(t, Term::BlankNode(_)))
                .collect();
            let work = graph::new_store();
            copy_into(&work, &source)?;
            if !isolated {
                copy_into(&work, catalog.store())?;
            }
            let inference = rules.execute(
                &work,
                RuleOptions {
                    iterate,
                    ..Default::default()
                },
            )?;
            // Source structure (lifted blank nodes) never leaks into the catalog.
            let output: Vec<_> = inference
                .produced
                .into_iter()
                .filter(|t| {
                    !source_nodes.contains(&t.subject.clone().into())
                        && !source_nodes.contains(&t.object)
                })
                .collect();
            if output.is_empty() {
                bail!("rules produced no catalog triples for {}", file.display());
            }
            let cs = if append {
                let mut cs = catalog.changeset();
                cs.added = output;
                cs
            } else {
                catalog.upsert(output)
            };
            if dry_run {
                println!("{}", graph::to_turtle(cs.added.clone())?);
                println!(
                    "# would add {} and remove {} triples",
                    cs.added.len(),
                    cs.removed.len()
                );
                print!("{}", catalog.preview(&cs)?);
                return Ok(ExitCode::SUCCESS);
            }
            let message = message.unwrap_or_else(|| format!("ingest {}", file.display()));
            return report_commit(catalog.commit(cs, &message));
        }
        Command::Load {
            file,
            message,
            delete,
        } => {
            let format =
                graph::parse_format(file.extension().and_then(|e| e.to_str()).unwrap_or("ttl"))?;
            let g = graph::new_store();
            graph::load(&g, read(&file)?.as_bytes(), format)?;
            let mut cs = catalog.changeset();
            if delete {
                cs.removed = graph::triples(&g);
            } else {
                cs.added = graph::triples(&g);
            }
            let message = message.unwrap_or_else(|| format!("load {}", file.display()));
            return report_commit(catalog.commit(cs, &message));
        }
        Command::Transform {
            rules,
            input,
            format,
            at,
            commit,
            message,
            iterate,
            output,
        } => {
            let rules = load_rules(&rules)?;
            let work = match &input {
                Some(f) => source_graph(f, format.as_deref(), 0)?,
                None => snapshot(&catalog, at)?,
            };
            let inference = rules.execute(
                &work,
                RuleOptions {
                    iterate,
                    ..Default::default()
                },
            )?;
            if commit {
                if input.is_some() || at.is_some() {
                    bail!("--commit only works on the current catalog");
                }
                let mut cs = catalog.changeset();
                cs.added = inference.inferred;
                let message = message.unwrap_or_else(|| "transform".to_owned());
                return report_commit(catalog.commit(cs, &message));
            }
            print!(
                "{}",
                graph::serialize(inference.inferred, graph::parse_format(&output)?)?
            );
        }
        Command::Lift { file, format, rows } => {
            print!(
                "{}",
                graph::to_turtle(graph::triples(&source_graph(
                    &file,
                    format.as_deref(),
                    rows
                )?))?
            );
        }
        Command::Validate {
            shapes,
            at,
            data,
            turtle,
        } => {
            let g = graph::new_store();
            graph::load_turtle(&g, queryrdf::catalog::CATALOG_SHAPES)?;
            for (_, ttl) in catalog.persisted_shapes()? {
                graph::load_turtle(&g, &ttl)?;
            }
            for p in &shapes {
                graph::load_turtle(&g, &read(p)?)?;
            }
            let shapes = ShapesGraph::from_store(g)?;
            let data = match data {
                Some(f) => {
                    let g = graph::new_store();
                    let format = graph::parse_format(
                        f.extension().and_then(|e| e.to_str()).unwrap_or("ttl"),
                    )?;
                    graph::load(&g, read(&f)?.as_bytes(), format)?;
                    g
                }
                None => snapshot(&catalog, at)?,
            };
            let report = Validator::new(&shapes, &data).validate()?;
            if turtle {
                print!("{}", graph::to_turtle(report.to_triples())?);
            } else {
                print!("{report}");
            }
            return Ok(if report.conforms() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(2)
            });
        }
        Command::Query { query, at } => {
            let text = if query == "-" {
                let mut s = String::new();
                std::io::stdin().read_to_string(&mut s)?;
                s
            } else if let Some(path) = query.strip_prefix('@') {
                read(&PathBuf::from(path))?
            } else {
                query
            };
            let text = with_default_prefixes(&text);
            let store = match at {
                Some(v) => catalog.at_version(v)?,
                None => snapshot(&catalog, None)?,
            };
            print_results(graph::query(&store, &text)?)?;
        }
        Command::Log => {
            for c in catalog.log()? {
                println!(
                    "v{:<4} {}  +{:<5} -{:<5} {}",
                    c.version,
                    graph::epoch_millis_to_iso(c.timestamp_ms),
                    c.added.len(),
                    c.removed.len(),
                    c.message
                );
            }
        }
        Command::Dump { at, output } => {
            let store = snapshot(&catalog, at)?;
            print!(
                "{}",
                graph::serialize(graph::triples(&store), graph::parse_format(&output)?)?
            );
        }
        Command::Shapes { command } => match command {
            ShapesCommand::Add { file, name, force } => {
                let name = name.unwrap_or_else(|| {
                    file.file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("shapes")
                        .to_owned()
                });
                let report = catalog.add_shapes(&name, &read(&file)?, force)?;
                if report.conforms() || force {
                    println!("shapes '{name}' added");
                    print!("{report}");
                } else {
                    eprintln!(
                        "current catalog violates '{name}' (use --force to add anyway)\n{report}"
                    );
                    return Ok(ExitCode::from(2));
                }
            }
            ShapesCommand::Remove { name } => {
                if !catalog.remove_shapes(&name)? {
                    bail!("no shapes named '{name}'");
                }
                println!("shapes '{name}' removed");
            }
            ShapesCommand::List => {
                println!("builtin:catalog");
                for name in catalog.shape_sets()? {
                    println!("{name}");
                }
            }
        },
    }
    Ok(ExitCode::SUCCESS)
}

fn with_default_prefixes(query: &str) -> String {
    let mut out = String::new();
    for (p, ns) in queryrdf::vocab::PREFIXES {
        if !query.contains(&format!("PREFIX {p}:")) && !query.contains(&format!("prefix {p}:")) {
            out.push_str(&format!("PREFIX {p}: <{ns}>\n"));
        }
    }
    out.push_str(query);
    out
}

fn print_results(results: QueryResults<'_>) -> Result<()> {
    match results {
        QueryResults::Boolean(b) => println!("{b}"),
        QueryResults::Graph(triples) => {
            let triples = triples.collect::<Result<Vec<_>, _>>()?;
            print!("{}", graph::serialize(triples, RdfFormat::Turtle)?);
        }
        QueryResults::Solutions(solutions) => {
            let variables = solutions.variables().to_vec();
            let rows: Vec<Vec<String>> = solutions
                .map(|s| {
                    s.map(|s| {
                        variables
                            .iter()
                            .map(|v| s.get(v).map(render).unwrap_or_default())
                            .collect()
                    })
                })
                .collect::<Result<_, _>>()?;
            let header: Vec<String> = variables.iter().map(|v| v.as_str().to_owned()).collect();
            let widths: Vec<usize> = (0..header.len())
                .map(|i| {
                    rows.iter()
                        .map(|r| r[i].chars().count())
                        .chain([header[i].len()])
                        .max()
                        .unwrap_or(0)
                })
                .collect();
            let line = |cells: &[String]| {
                cells
                    .iter()
                    .zip(&widths)
                    .map(|(c, w)| format!("{c:<w$}"))
                    .collect::<Vec<_>>()
                    .join(" | ")
            };
            println!("{}", line(&header));
            println!(
                "{}",
                widths
                    .iter()
                    .map(|w| "-".repeat(*w))
                    .collect::<Vec<_>>()
                    .join("-+-")
            );
            for r in &rows {
                println!("{}", line(r));
            }
            println!("({} rows)", rows.len());
        }
    }
    Ok(())
}

fn render(t: &Term) -> String {
    match t {
        Term::Literal(l) => l.value().to_owned(),
        other => compact(other),
    }
}
