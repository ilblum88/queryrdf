//! Namespace constants used across the catalog.

pub use oxigraph::model::vocab::{rdf, rdfs, xsd};

macro_rules! namespace {
    ($(#[$doc:meta])* $module:ident, $base:literal, { $($name:ident = $local:literal),* $(,)? }) => {
        $(#[$doc])*
        pub mod $module {
            use oxigraph::model::NamedNodeRef;
            pub const NS: &str = $base;
            $(pub const $name: NamedNodeRef<'static> = NamedNodeRef::new_unchecked(concat!($base, $local));)*
        }
    };
}

namespace!(
    /// W3C SHACL and SHACL Advanced Features.
    sh, "http://www.w3.org/ns/shacl#", {
    NODE_SHAPE = "NodeShape", PROPERTY_SHAPE = "PropertyShape",
    PATH = "path", INVERSE_PATH = "inversePath", ALTERNATIVE_PATH = "alternativePath",
    ZERO_OR_MORE_PATH = "zeroOrMorePath", ONE_OR_MORE_PATH = "oneOrMorePath", ZERO_OR_ONE_PATH = "zeroOrOnePath",
    TARGET_CLASS = "targetClass", TARGET_NODE = "targetNode", TARGET_SUBJECTS_OF = "targetSubjectsOf",
    TARGET_OBJECTS_OF = "targetObjectsOf", TARGET = "target", SPARQL_TARGET = "SPARQLTarget",
    SELECT = "select", ASK = "ask", CONSTRUCT = "construct", PREFIXES = "prefixes", DECLARE = "declare",
    PREFIX = "prefix", NAMESPACE = "namespace",
    PROPERTY = "property", NODE = "node", CLASS = "class", DATATYPE = "datatype", NODE_KIND = "nodeKind",
    IRI = "IRI", BLANK_NODE = "BlankNode", LITERAL = "Literal", BLANK_NODE_OR_IRI = "BlankNodeOrIRI",
    BLANK_NODE_OR_LITERAL = "BlankNodeOrLiteral", IRI_OR_LITERAL = "IRIOrLiteral",
    MIN_COUNT = "minCount", MAX_COUNT = "maxCount", MIN_EXCLUSIVE = "minExclusive", MIN_INCLUSIVE = "minInclusive",
    MAX_EXCLUSIVE = "maxExclusive", MAX_INCLUSIVE = "maxInclusive", MIN_LENGTH = "minLength", MAX_LENGTH = "maxLength",
    PATTERN = "pattern", FLAGS = "flags", LANGUAGE_IN = "languageIn", UNIQUE_LANG = "uniqueLang",
    EQUALS = "equals", DISJOINT = "disjoint", LESS_THAN = "lessThan", LESS_THAN_OR_EQUALS = "lessThanOrEquals",
    NOT = "not", AND = "and", OR = "or", XONE = "xone",
    QUALIFIED_VALUE_SHAPE = "qualifiedValueShape", QUALIFIED_MIN_COUNT = "qualifiedMinCount",
    QUALIFIED_MAX_COUNT = "qualifiedMaxCount", QUALIFIED_VALUE_SHAPES_DISJOINT = "qualifiedValueShapesDisjoint",
    CLOSED = "closed", IGNORED_PROPERTIES = "ignoredProperties", HAS_VALUE = "hasValue", IN = "in",
    SPARQL = "sparql", MESSAGE = "message", SEVERITY = "severity", VIOLATION = "Violation", WARNING = "Warning",
    INFO = "Info", DEACTIVATED = "deactivated",
    VALIDATION_REPORT = "ValidationReport", VALIDATION_RESULT = "ValidationResult", CONFORMS = "conforms",
    RESULT = "result", FOCUS_NODE = "focusNode", RESULT_PATH = "resultPath", VALUE = "value",
    SOURCE_SHAPE = "sourceShape", SOURCE_CONSTRAINT_COMPONENT = "sourceConstraintComponent",
    SOURCE_CONSTRAINT = "sourceConstraint", RESULT_SEVERITY = "resultSeverity", RESULT_MESSAGE = "resultMessage",
    RULE = "rule", TRIPLE_RULE = "TripleRule", SPARQL_RULE = "SPARQLRule", SUBJECT = "subject",
    PREDICATE = "predicate", OBJECT = "object", ORDER = "order", CONDITION = "condition", THIS = "this",
    FILTER_SHAPE = "filterShape", NODES = "nodes", INTERSECTION = "intersection", UNION = "union",
    SPARQL_FUNCTION = "SPARQLFunction", PARAMETER = "parameter", OPTIONAL = "optional",
});

namespace!(
    /// The queryrdf catalog model (Iceberg / DuckLake inspired).
    qc, "https://w3id.org/queryrdf/catalog#", {
    CATALOG = "Catalog", NAMESPACE = "Namespace", TABLE = "Table", SCHEMA = "Schema", FIELD = "Field",
    SNAPSHOT = "Snapshot", DATA_FILE = "DataFile", COLUMN_STATS = "ColumnStats", PARTITION_FIELD = "PartitionField",
    NAME = "name", IN_CATALOG = "inCatalog", IN_NAMESPACE = "inNamespace", LOCATION = "location", UUID = "uuid",
    FORMAT_VERSION = "formatVersion", SCHEMA_PROP = "schema", CURRENT_SCHEMA = "currentSchema", SCHEMA_ID = "schemaId",
    FIELD_PROP = "field", FIELD_ID = "fieldId", DATA_TYPE = "dataType", REQUIRED = "required", POSITION = "position",
    SNAPSHOT_PROP = "snapshot", CURRENT_SNAPSHOT = "currentSnapshot", SNAPSHOT_ID = "snapshotId",
    PARENT_SNAPSHOT = "parentSnapshot", TIMESTAMP = "timestamp", OPERATION = "operation",
    DATA_FILE_PROP = "dataFile", FILE_PATH = "filePath", FILE_FORMAT = "fileFormat", RECORD_COUNT = "recordCount",
    FILE_SIZE_BYTES = "fileSizeBytes", COLUMN_STATS_PROP = "columnStats", COLUMN = "column", MIN_VALUE = "minValue",
    MAX_VALUE = "maxValue", NULL_COUNT = "nullCount", CONCEPT = "concept",
});

namespace!(
    /// Structural vocabulary produced when lifting non-RDF sources.
    qs, "https://w3id.org/queryrdf/source#", {
    DOCUMENT = "Document", ARRAY = "Array", ITEM = "item", INDEX = "index", SOURCE_FILE = "sourceFile",
    FILE_NAME = "fileName", FORMAT = "format", FILE_SIZE_BYTES = "fileSizeBytes", COLUMN_CLASS = "Column",
    COLUMN = "column", NAME = "name", POSITION = "position", INFERRED_TYPE = "inferredType", ROW_COUNT = "rowCount",
    NULL_COUNT = "nullCount", MIN_VALUE = "minValue", MAX_VALUE = "maxValue", ROW_CLASS = "Row", ROW = "row", MODIFIED = "modified",
    HEADER_SIGNATURE = "headerSignature",
});

namespace!(
    /// Built-in SPARQL functions.
    qf, "https://w3id.org/queryrdf/function#", {
    EPOCH_MILLIS_TO_DATE_TIME = "epochMillisToDateTime", HASH64 = "hash64",
});

/// Namespace for JSON object keys lifted into predicates.
pub const JSON_NS: &str = "https://w3id.org/queryrdf/json#";

/// Prefixes used when serializing Turtle.
pub const PREFIXES: &[(&str, &str)] = &[
    ("rdf", "http://www.w3.org/1999/02/22-rdf-syntax-ns#"),
    ("rdfs", "http://www.w3.org/2000/01/rdf-schema#"),
    ("xsd", "http://www.w3.org/2001/XMLSchema#"),
    ("sh", sh::NS),
    ("qc", qc::NS),
    ("qs", qs::NS),
    ("qf", qf::NS),
    ("j", JSON_NS),
    ("dcat", "http://www.w3.org/ns/dcat#"),
    ("dct", "http://purl.org/dc/terms/"),
    ("prov", "http://www.w3.org/ns/prov#"),
    ("schema", "https://schema.org/"),
];
