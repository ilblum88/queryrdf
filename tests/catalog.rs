use std::process::Command;

use oxigraph::model::*;
use queryrdf::catalog::{Catalog, CommitError};
use queryrdf::shacl::{ShapesGraph, Validator};
use queryrdf::{RuleOptions, RuleSet, graph};

const QC: &str = "https://w3id.org/queryrdf/catalog#";

fn qc(local: &str) -> NamedNode {
    NamedNode::new(format!("{QC}{local}")).unwrap()
}

fn cli(db: &std::path::Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_queryrdf"))
        .arg("--db")
        .arg(db)
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn namespace(cat: &Catalog, name: &str) -> Vec<Triple> {
    let ns = NamedNode::new(format!("urn:test:ns:{name}")).unwrap();
    let _ = cat;
    vec![
        Triple::new(ns.clone(), vocab_type(), qc("Namespace")),
        Triple::new(ns, qc("name"), Literal::from(name)),
    ]
}

fn vocab_type() -> NamedNode {
    oxigraph::model::vocab::rdf::TYPE.into_owned()
}

#[test]
fn commits_persist_and_time_travel() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("c.redb");
    {
        let mut cat = Catalog::open(&db).unwrap();
        let mut cs = cat.changeset();
        cs.added = namespace(&cat, "sales");
        assert_eq!(cat.commit(cs, "one").unwrap().version, 1);
        let mut cs = cat.changeset();
        cs.added = namespace(&cat, "hr");
        cs.removed = namespace(&cat, "sales");
        assert_eq!(cat.commit(cs, "two").unwrap().version, 2);
    }
    let cat = Catalog::open(&db).unwrap();
    assert_eq!(cat.version(), 2);
    assert_eq!(graph::triples(cat.store()).len(), 2);
    let v1 = cat.at_version(1).unwrap();
    assert!(graph::ask(&v1, &format!("ASK {{ ?n <{QC}name> \"sales\" }}"), &[]).unwrap());
    assert!(
        !graph::ask(
            cat.store(),
            &format!("ASK {{ ?n <{QC}name> \"sales\" }}"),
            &[]
        )
        .unwrap()
    );
    assert!(graph::triples(&cat.at_version(0).unwrap()).is_empty());
    assert!(cat.at_version(3).is_err());
    let log = cat.log().unwrap();
    assert_eq!(
        log.iter().map(|c| c.message.as_str()).collect::<Vec<_>>(),
        ["one", "two"]
    );
}

#[test]
fn stale_changeset_is_a_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let mut cat = Catalog::open(dir.path().join("c.redb")).unwrap();
    let mut stale = cat.changeset();
    stale.added = namespace(&cat, "a");
    let mut cs = cat.changeset();
    cs.added = namespace(&cat, "b");
    cat.commit(cs, "first").unwrap();
    match cat.commit(stale, "stale") {
        Err(CommitError::Conflict {
            base: 0,
            current: 1,
        }) => {}
        other => panic!("expected conflict, got {other:?}"),
    }
    assert_eq!(cat.version(), 1);
}

#[test]
fn invalid_commit_is_rejected_and_rolled_back() {
    let dir = tempfile::tempdir().unwrap();
    let mut cat = Catalog::open(dir.path().join("c.redb")).unwrap();
    let ns = NamedNode::new("urn:test:ns:bad").unwrap();
    let mut cs = cat.changeset();
    cs.add(Triple::new(ns.clone(), vocab_type(), qc("Namespace")));
    cs.add(Triple::new(ns, qc("name"), Literal::from("has space")));
    match cat.commit(cs, "bad") {
        Err(CommitError::Invalid(report)) => assert!(!report.conforms()),
        other => panic!("expected validation failure, got {other:?}"),
    }
    assert_eq!(cat.version(), 0);
    assert!(graph::triples(cat.store()).is_empty());
}

#[test]
fn upsert_replaces_values() {
    let dir = tempfile::tempdir().unwrap();
    let mut cat = Catalog::open(dir.path().join("c.redb")).unwrap();
    let mut cs = cat.changeset();
    cs.added = namespace(&cat, "old");
    cat.commit(cs, "init").unwrap();
    let ns = NamedNode::new("urn:test:ns:old").unwrap();
    let cs = cat.upsert([Triple::new(ns, qc("name"), Literal::from("new"))]);
    assert_eq!((cs.added.len(), cs.removed.len()), (1, 1));
    cat.commit(cs, "rename").unwrap();
    assert!(
        graph::ask(
            cat.store(),
            &format!("ASK {{ ?n <{QC}name> \"new\" }}"),
            &[]
        )
        .unwrap()
    );
    assert_eq!(graph::triples(cat.store()).len(), 2);
}

#[test]
fn persisted_shapes_apply_to_later_commits() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("c.redb");
    {
        let mut cat = Catalog::open(&db).unwrap();
        let shapes = format!(
            "@prefix sh: <http://www.w3.org/ns/shacl#> .
             <urn:s> a sh:NodeShape ; sh:targetClass <{QC}Namespace> ;
               sh:property [ sh:path <{QC}name> ; sh:in (\"sales\" \"hr\") ] ."
        );
        assert!(
            cat.add_shapes("allowed", &shapes, false)
                .unwrap()
                .conforms()
        );
    }
    let mut cat = Catalog::open(&db).unwrap();
    assert_eq!(cat.shape_sets().unwrap(), ["allowed"]);
    let mut cs = cat.changeset();
    cs.added = namespace(&cat, "marketing");
    assert!(matches!(cat.commit(cs, "x"), Err(CommitError::Invalid(_))));
    let mut cs = cat.changeset();
    cs.added = namespace(&cat, "hr");
    cat.commit(cs, "y").unwrap();
}

#[test]
fn shacl_core_constraints() {
    let shapes = ShapesGraph::from_turtle(
        r#"@prefix sh: <http://www.w3.org/ns/shacl#> . @prefix ex: <http://ex/> .
           @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
           ex:P a sh:NodeShape ; sh:targetClass ex:Person ; sh:closed true ; sh:ignoredProperties ( <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> ) ;
             sh:property [ sh:path ex:age ; sh:datatype xsd:integer ; sh:maxInclusive 150 ; sh:maxCount 1 ] ;
             sh:property [ sh:path ex:knows ; sh:class ex:Person ] ;
             sh:property [ sh:path ( ex:knows ex:age ) ; sh:minInclusive 18 ] ;
             sh:property [ sh:path ex:start ; sh:lessThan ex:end ] ;
             sh:property [ sh:path ex:end ] ;
             sh:property [ sh:path ex:email ; sh:or ( [ sh:pattern "@" ] [ sh:nodeKind sh:IRI ] ) ] ."#,
    )
    .unwrap();
    let data = graph::new_store();
    graph::load_turtle(
        &data,
        r#"@prefix ex: <http://ex/> . @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
           ex:a a ex:Person ; ex:age 30 ; ex:knows ex:b ; ex:start 1 ; ex:end 2 ; ex:email "a@x" .
           ex:b a ex:Person ; ex:age 200, 12 ; ex:knows ex:c ; ex:start 5 ; ex:end 3 ; ex:email "nope" ; ex:extra 1 .
           ex:c ex:age "x"^^xsd:integer ."#,
    )
    .unwrap();
    let report = Validator::new(&shapes, &data).validate().unwrap();
    let mut got: Vec<(String, String)> = report
        .results
        .iter()
        .map(|r| {
            (
                graph::local_name(&r.focus.to_string())
                    .trim_end_matches('>')
                    .to_owned(),
                graph::local_name(r.component.as_str())
                    .trim_end_matches("ConstraintComponent")
                    .to_owned(),
            )
        })
        .collect();
    got.sort();
    let expected = [
        ("a", "MinInclusive"),
        ("b", "Class"),
        ("b", "Closed"),
        ("b", "LessThan"),
        ("b", "MaxCount"),
        ("b", "MaxInclusive"),
        ("b", "MinInclusive"),
        ("b", "Or"),
    ];
    let expected: Vec<(String, String)> = expected
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
    assert_eq!(got, expected, "{report}");
}

#[test]
fn shacl_af_rules_with_order_condition_and_iteration() {
    let rules = graph::new_store();
    graph::load_turtle(
        &rules,
        r#"@prefix sh: <http://www.w3.org/ns/shacl#> . @prefix ex: <http://ex/> .
           ex:S a sh:NodeShape ; sh:targetClass ex:Person ;
             sh:rule [ a sh:TripleRule ; sh:order 1 ; sh:subject sh:this ; sh:predicate ex:label ;
                       sh:object [ sh:path ex:name ] ] ;
             sh:rule [ a sh:SPARQLRule ; sh:order 2 ; sh:condition ex:Adult ;
                       sh:construct "CONSTRUCT { $this a <http://ex/Adult> } WHERE { $this <http://ex/label> ?l }" ] .
           ex:Adult a sh:NodeShape ; sh:property [ sh:path ex:age ; sh:minInclusive 18 ] .
           ex:T a sh:NodeShape ; sh:targetSubjectsOf ex:parent ;
             sh:rule [ a sh:TripleRule ; sh:subject sh:this ; sh:predicate ex:ancestor ;
                       sh:object [ sh:path ( ex:parent [ sh:zeroOrMorePath ex:ancestor ] ) ] ] ."#,
    )
    .unwrap();
    let rules = RuleSet::from_store(rules).unwrap();
    let data = graph::new_store();
    graph::load_turtle(
        &data,
        r#"@prefix ex: <http://ex/> .
           ex:a a ex:Person ; ex:name "A" ; ex:age 40 ; ex:parent ex:b .
           ex:b a ex:Person ; ex:name "B" ; ex:age 10 ; ex:parent ex:c .
           ex:c ex:parent ex:d ."#,
    )
    .unwrap();
    let inf = rules
        .execute(
            &data,
            RuleOptions {
                iterate: true,
                ..Default::default()
            },
        )
        .unwrap();
    let ask = |q: &str| {
        graph::ask(
            &data,
            &format!("PREFIX ex: <http://ex/> ASK {{ {q} }}"),
            &[],
        )
        .unwrap()
    };
    assert!(ask("ex:a ex:label \"A\" ; a ex:Adult"));
    assert!(ask("ex:b ex:label \"B\""));
    assert!(!ask("ex:b a ex:Adult"));
    assert!(ask("ex:a ex:ancestor ex:b, ex:c, ex:d"));
    assert!(!inf.inferred.is_empty());
}

#[test]
fn cli_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("c.redb");
    let ok = |args: &[&str]| {
        let (code, out, err) = cli(&db, args);
        assert_eq!(code, 0, "{args:?}\n{out}\n{err}");
        out
    };
    ok(&["init"]);
    ok(&[
        "ingest",
        "examples/iceberg/v1.metadata.json",
        "-r",
        "rules/iceberg.ttl",
    ]);
    ok(&[
        "ingest",
        "examples/iceberg/v2.metadata.json",
        "-r",
        "rules/iceberg.ttl",
    ]);
    let (code, _, err) = cli(
        &db,
        &[
            "ingest",
            "examples/iceberg/broken.metadata.json",
            "-r",
            "rules/iceberg.ttl",
        ],
    );
    assert_eq!(code, 2);
    assert!(err.contains("is not one of the table's snapshots"), "{err}");
    ok(&[
        "ingest",
        "examples/csv/customers.csv",
        "-r",
        "rules/csv.ttl",
    ]);
    ok(&["transform", "-r", "rules/annotate.ttl", "--commit"]);
    ok(&["validate"]);

    let out = ok(&[
        "query",
        "SELECT ?p WHERE { ?s qc:snapshotId 8333017788700497002 ; qc:parentSnapshot/qc:snapshotId ?p }",
    ]);
    assert!(out.contains("3051729675574597004"), "{out}");
    let out = ok(&[
        "query",
        "--at",
        "1",
        "SELECT (COUNT(*) AS ?n) WHERE { ?s a qc:Schema }",
    ]);
    assert!(out.lines().nth(2).unwrap().trim() == "1", "{out}");
    let out = ok(&[
        "query",
        "SELECT ?c WHERE { ?f qc:name \"email\" ; qc:concept ?c }",
    ]);
    assert!(out.contains("schema:email"), "{out}");
    let out = ok(&["transform", "-r", "rules/dcat.ttl"]);
    assert!(
        out.contains("dcat:Dataset") && out.contains("dcat:distribution"),
        "{out}"
    );
    let log = ok(&["log"]);
    assert_eq!(log.lines().count(), 4, "{log}");
}

#[test]
fn shapes_extending_builtin_shapes_are_checked_merged() {
    let dir = tempfile::tempdir().unwrap();
    let mut cat = Catalog::open(dir.path().join("c.redb")).unwrap();
    let mut cs = cat.changeset();
    cs.added = namespace(&cat, "sales");
    cat.commit(cs, "ns").unwrap();
    let ext = format!(
        "@prefix sh: <http://www.w3.org/ns/shacl#> .
         <https://w3id.org/queryrdf/shapes#NamespaceShape> sh:property [ sh:path <{QC}owner> ; sh:minCount 1 ] ."
    );
    assert!(!cat.add_shapes("ext", &ext, false).unwrap().conforms());
    assert!(cat.shape_sets().unwrap().is_empty());
}

#[test]
fn sparql_functions_in_rules_see_the_data_graph() {
    let rules = graph::new_store();
    graph::load_turtle(
        &rules,
        r#"@prefix sh: <http://www.w3.org/ns/shacl#> . @prefix ex: <http://ex/> .
           ex:labelOf a sh:SPARQLFunction ; sh:parameter [ sh:path ex:node ] ;
             sh:select "SELECT ?result WHERE { $node <http://ex/name> ?result }" .
           ex:S a sh:NodeShape ; sh:targetNode ex:a ;
             sh:rule [ a sh:SPARQLRule ;
               sh:construct "CONSTRUCT { $this <http://ex/label> ?l } WHERE { $this <http://ex/name> ?n . BIND(<http://ex/labelOf>($this) AS ?l) }" ] ;
             sh:rule [ a sh:TripleRule ; sh:subject sh:this ; sh:predicate ex:label2 ;
               sh:object [ ex:labelOf ( sh:this ) ] ] ."#,
    )
    .unwrap();
    let rules = RuleSet::from_store(rules).unwrap();
    let data = graph::new_store();
    graph::load_turtle(&data, r#"<http://ex/a> <http://ex/name> "A" ."#).unwrap();
    rules.execute(&data, RuleOptions::default()).unwrap();
    let ask = |q: &str| graph::ask(&data, &format!("ASK {{ {q} }}"), &[]).unwrap();
    assert!(ask(
        r#"<http://ex/a> <http://ex/label> "A" ; <http://ex/label2> "A""#
    ));
}

#[test]
fn non_terminating_rules_fail_in_iterate_mode() {
    let rules = graph::new_store();
    graph::load_turtle(
        &rules,
        r#"@prefix sh: <http://www.w3.org/ns/shacl#> . @prefix ex: <http://ex/> .
           ex:S a sh:NodeShape ; sh:targetSubjectsOf ex:n ;
             sh:rule [ a sh:SPARQLRule ;
               sh:construct "CONSTRUCT { $this <http://ex/n> ?m } WHERE { $this <http://ex/n> ?k . BIND(?k + 1 AS ?m) }" ] ."#,
    )
    .unwrap();
    let rules = RuleSet::from_store(rules).unwrap();
    let data = graph::new_store();
    graph::load_turtle(&data, "<http://ex/a> <http://ex/n> 0 .").unwrap();
    let err = rules
        .execute(
            &data,
            RuleOptions {
                iterate: true,
                max_iterations: 5,
            },
        )
        .unwrap_err();
    assert!(err.to_string().contains("fixpoint"), "{err}");
}

#[test]
fn cli_source_edge_cases() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("c.redb");
    let ok = |args: &[&str]| {
        let (code, out, err) = cli(&db, args);
        assert_eq!(code, 0, "{args:?}\n{out}\n{err}");
        out
    };

    // Iceberg metadata whose child snapshot references an expired parent.
    let mut meta: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/examples/iceberg/v2.metadata.json"
        ))
        .unwrap(),
    )
    .unwrap();
    meta["snapshots"].as_array_mut().unwrap().remove(0);
    let expired = dir.path().join("expired.json");
    std::fs::write(&expired, meta.to_string()).unwrap();
    ok(&[
        "ingest",
        expired.to_str().unwrap(),
        "-r",
        "rules/iceberg.ttl",
    ]);

    // CSV schema evolution keeps the earlier snapshot's schema; offsets order timestamps.
    let v1 = dir.path().join("v1");
    let v2 = dir.path().join("v2");
    std::fs::create_dir_all(&v1).unwrap();
    std::fs::create_dir_all(&v2).unwrap();
    std::fs::write(
        v1.join("events.csv"),
        "id,ts\n1,2024-01-01T10:00:00+05:00\n2,2024-01-01T06:00:00Z\n",
    )
    .unwrap();
    std::fs::write(
        v2.join("events.csv"),
        "id,ts,kind\n3,2024-01-02T00:00:00Z,a\n",
    )
    .unwrap();
    ok(&[
        "ingest",
        v1.join("events.csv").to_str().unwrap(),
        "-r",
        "rules/csv.ttl",
    ]);
    ok(&[
        "ingest",
        v2.join("events.csv").to_str().unwrap(),
        "-r",
        "rules/csv.ttl",
    ]);
    let out = ok(&[
        "query",
        "SELECT ?n (COUNT(?f) AS ?fields) WHERE { ?s qc:recordCount ?n . ?snap qc:dataFile ?s ; qc:schema/qc:field ?f } GROUP BY ?n ORDER BY ?n",
    ]);
    let rows: Vec<&str> = out.lines().skip(2).take(2).map(|l| l.trim()).collect();
    assert_eq!(rows, ["1 | 3", "2 | 2"], "{out}");
    let out = ok(&[
        "query",
        "SELECT ?min WHERE { ?st qc:column/qc:name \"ts\" ; qc:minValue ?min }",
    ]);
    assert!(out.contains("2024-01-01T10:00:00+05:00"), "{out}");

    // RDF sources keep their blank nodes.
    let ttl = dir.path().join("ns.ttl");
    std::fs::write(&ttl, "[] <http://ex/label> \"blank\" .").unwrap();
    let rules = dir.path().join("rdf.ttl");
    std::fs::write(
        &rules,
        r#"@prefix sh: <http://www.w3.org/ns/shacl#> .
           <urn:r> a sh:NodeShape ; sh:targetSubjectsOf <http://ex/label> ;
             sh:rule [ a sh:TripleRule ; sh:subject sh:this ; sh:predicate <http://ex/copy> ;
                       sh:object [ sh:path <http://ex/label> ] ] ."#,
    )
    .unwrap();
    let out = ok(&[
        "ingest",
        ttl.to_str().unwrap(),
        "-r",
        rules.to_str().unwrap(),
    ]);
    assert!(out.contains("(+1 "), "{out}");
}
