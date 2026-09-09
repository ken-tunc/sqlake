//! The tool list, from the protocol types.
//!
//! Every tool is one `RequestKind`, and its input schema is that request's own
//! schema with the tag taken out — the tool's name carries it, so a caller
//! cannot write the wrong one. Generated rather than written, for the reason
//! `api schema` is: a second description of the surface drifts, and it drifts
//! by going on describing a call that has changed.

use rmcp::model::{JsonObject, Tool};
use serde_json::{Map, Value as Json};
use sqlake_api::{RequestKind, schema};

/// Every tool this server offers, in the order `RequestKind` lists them.
#[must_use]
pub fn all() -> Vec<Tool> {
    let document = schema();
    RequestKind::ALL
        .iter()
        .filter(|kind| offered(**kind))
        .map(|kind| crate::tool_with(*kind, from_document(&document, *kind)))
        .collect()
}

/// The request a tool name means, if it is one of ours.
#[must_use]
pub fn kind_of(name: &str) -> Option<RequestKind> {
    RequestKind::ALL
        .iter()
        .copied()
        .find(|kind| offered(*kind) && kind.tag() == name)
}

/// Whether this request is worth being a tool.
///
/// `schema` is not: `tools/list` *is* the schema for an MCP client, so
/// offering the document as well would be the same surface described twice,
/// and the second copy is the one nobody keeps reading.
const fn offered(kind: RequestKind) -> bool {
    !matches!(kind, RequestKind::Schema)
}

/// Whether this request changes anything.
///
/// A hint on the tool, so a client told to be careful can be — and false for
/// `query_run` would be a lie, because what it runs is whatever SQL it was
/// handed.
#[must_use]
pub const fn writes(kind: RequestKind) -> bool {
    matches!(
        kind,
        RequestKind::ConnectionOpen
            | RequestKind::ConnectionClose
            | RequestKind::QueryRun
            | RequestKind::QueryCancel
    )
}

/// Whether getting this one wrong cannot be taken back.
///
/// Closing a connection and cancelling a query both destroy something somebody
/// else may be using — a shared session is the case the whole surface exists
/// for. It is also what stops the server guessing which connection to close
/// when a tool names none.
#[must_use]
pub const fn is_destructive(kind: RequestKind) -> bool {
    matches!(
        kind,
        RequestKind::ConnectionClose | RequestKind::QueryCancel
    )
}

/// Whether this request names a connection the server can choose.
#[must_use]
pub const fn takes_a_connection(kind: RequestKind) -> bool {
    matches!(
        kind,
        RequestKind::NamespaceList
            | RequestKind::TableList
            | RequestKind::TablePreview
            | RequestKind::TableDescribe
            // Optional here, unlike everywhere else: filling a template in
            // needs quoting rules rather than a database, and there is a
            // sensible answer with nothing open at all.
            | RequestKind::TemplateApply
            | RequestKind::ConnectionClose
            | RequestKind::QueryEstimate
            | RequestKind::QueryRun
    )
}

/// What the tool is for, in one line an agent reads before choosing it.
#[must_use]
pub const fn describes(kind: RequestKind) -> &'static str {
    match kind {
        RequestKind::Snapshot => {
            "Everything this session has open: its connections and the profiles it could open."
        }
        RequestKind::Schema => "The request and response schema for this surface.",
        RequestKind::ConnectionList => "The connections this session has open, with their ids.",
        RequestKind::ConnectionOpen => "Open a connection to a configured profile.",
        RequestKind::ConnectionClose => {
            "Close one of the session's connections. Its cached tree and results go with it."
        }
        RequestKind::NamespaceList => {
            "The namespaces in a connection — what the driver calls a schema or a dataset."
        }
        RequestKind::TableList => "The relations in one namespace.",
        RequestKind::TablePreview => {
            "A page of one relation, read without running a query where the driver allows it."
        }
        RequestKind::TemplateList => {
            "The statements saved in this session's library, with the placeholders each one \
             asks for."
        }
        RequestKind::TemplateApply => {
            "Fill a saved statement's placeholders in and get the SQL back. It is not run: \
             pass the answer to query_run when you want that."
        }
        RequestKind::TableDescribe => {
            "What a relation is rather than what is in it: columns, indexes, and a CREATE \
             statement built from the catalogue. Cheaper than reading a page to find the \
             column names."
        }
        RequestKind::QueryEstimate => {
            "What a statement would cost, without running it. Show this to a person before running something expensive."
        }
        RequestKind::QueryRun => "Run a statement. Answers a handle; use query_wait for the rows.",
        RequestKind::QueryStatus => "Where a query has got to, answered at once.",
        RequestKind::QueryWait => "Block until a query finishes, fails, or the wait runs out.",
        RequestKind::QueryCancel => "Stop a query this session started.",
    }
}

/// The schema for one tool's arguments.
///
/// The request's own branch of the generated schema, with two edits and no
/// third: the `request` tag goes, because the tool's name is it; and
/// `connection` stops being required, because the server picks one the way a
/// subcommand does. Anything else would be this crate describing the protocol
/// rather than quoting it.
#[must_use]
pub fn input_schema(kind: RequestKind) -> JsonObject {
    from_document(&schema(), kind)
}

/// The same, against a document generated once.
///
/// [`all`] wants one per tool, and `schema()` rebuilds the whole protocol
/// document each time it is called.
pub(crate) fn from_document(document: &Json, kind: RequestKind) -> JsonObject {
    let defs = document.get("$defs").cloned().unwrap_or(Json::Null);
    let mut branch = branch_for(document, kind).unwrap_or_else(|| {
        // A kind with no branch means the generated schema and `RequestKind`
        // disagree, which `the_schema_describes_every_request_and_no_others`
        // in `sqlake-api` already forbids. An empty object here would be a
        // tool that accepts anything, so it says so instead.
        let mut object = Map::new();
        object.insert("type".into(), Json::from("object"));
        object.insert(
            "description".into(),
            Json::from(format!("no schema was generated for `{}`", kind.tag())),
        );
        object
    });

    if let Some(Json::Object(properties)) = branch.get_mut("properties") {
        properties.remove("request");
        if let Some(Json::Object(connection)) = properties.get_mut("connection") {
            connection.insert(
                "description".into(),
                Json::from(
                    "A connection id from connection_list. Omit it to use the session's \
                     connection, which is what a subcommand does.",
                ),
            );
        }
    }
    if let Some(Json::Array(required)) = branch.get_mut("required") {
        required.retain(|name| name != "request" && name != "connection");
    }
    // The `$ref`s inside a branch are written `#/$defs/…`, and `#` is now this
    // document's root rather than the one they were generated under — so the
    // definitions have to come along. Only the ones reached, though: the
    // document's `$defs` is mostly *response* types, and carrying all of it
    // onto every tool would put a hundred kilobytes of schema an agent
    // cannot call in front of it, which is the context this surface exists to
    // spend carefully.
    if let Some(defs) = defs.as_object() {
        let reached = reachable(&Json::Object(branch.clone()), defs);
        if !reached.is_empty() {
            branch.insert("$defs".into(), Json::Object(reached));
        }
    }
    branch
}

/// The definitions a schema names, and the ones those name in turn.
///
/// Transitive because a definition is free to refer to another, and a `$ref`
/// left dangling is worse than the bulk it saved: a client that validates would
/// reject every call.
fn reachable(from: &Json, defs: &Map<String, Json>) -> Map<String, Json> {
    let mut kept = Map::new();
    let mut pending = vec![from.clone()];
    while let Some(node) = pending.pop() {
        for name in refs(&node) {
            if let Some(definition) = defs.get(&name)
                && kept.insert(name, definition.clone()).is_none()
            {
                pending.push(definition.clone());
            }
        }
    }
    kept
}

/// Every `#/$defs/<name>` under a node.
fn refs(node: &Json) -> Vec<String> {
    match node {
        Json::Object(fields) => {
            let mut found: Vec<String> = fields
                .get("$ref")
                .and_then(Json::as_str)
                .and_then(|r| r.strip_prefix("#/$defs/"))
                .map(|name| vec![name.to_owned()])
                .unwrap_or_default();
            found.extend(fields.values().flat_map(refs));
            found
        }
        Json::Array(items) => items.iter().flat_map(refs).collect(),
        _ => Vec::new(),
    }
}

/// The `oneOf` branch describing one request.
///
/// Matched on the tag's `const`, which is what an internally tagged enum puts
/// in each branch — rather than on position, which would silently pair a tool
/// with the wrong schema the first time a variant was reordered.
fn branch_for(document: &Json, kind: RequestKind) -> Option<Map<String, Json>> {
    let branches = document.get("request")?.get("oneOf")?.as_array()?;
    branches
        .iter()
        .find(|branch| {
            branch
                .get("properties")
                .and_then(|p| p.get("request"))
                .and_then(|r| r.get("const"))
                .and_then(Json::as_str)
                == Some(kind.tag())
        })
        .and_then(|branch| branch.as_object().cloned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_request_but_the_schema_is_a_tool() {
        let offered_tools = all();
        let names: Vec<&str> = offered_tools.iter().map(|t| t.name.as_ref()).collect();
        assert_eq!(names.len(), RequestKind::ALL.len() - 1);
        assert!(!names.contains(&"schema"), "{names:?}");
        // And the surface is complete: an agent that can only see tools has to
        // be able to reach everything a subcommand reaches.
        for kind in RequestKind::ALL {
            assert_eq!(
                names.contains(&kind.tag()),
                offered(*kind),
                "{} is offered: {}",
                kind.tag(),
                offered(*kind)
            );
        }
    }

    #[test]
    fn a_tool_name_is_the_requests_own_tag() {
        // Which is what lets a call be turned into a request without this
        // crate holding a second table of names.
        for kind in RequestKind::ALL {
            if offered(*kind) {
                assert_eq!(kind_of(kind.tag()), Some(*kind));
            }
        }
        assert_eq!(kind_of("schema"), None, "not offered, so not callable");
        assert_eq!(kind_of("nonsense"), None);
    }

    #[test]
    fn a_tools_schema_is_the_requests_schema_without_its_tag() {
        let schema = input_schema(RequestKind::TableList);
        let properties = schema["properties"].as_object().expect("properties");
        assert!(
            !properties.contains_key("request"),
            "the tool's name is the tag: {properties:?}"
        );
        assert!(properties.contains_key("namespace"));
        assert!(properties.contains_key("connection"));

        let required: Vec<&str> = schema["required"]
            .as_array()
            .expect("required")
            .iter()
            .filter_map(Json::as_str)
            .collect();
        assert_eq!(required, ["namespace"], "the server picks the connection");
    }

    #[test]
    fn a_schema_that_refers_to_a_definition_carries_it() {
        // `$ref`s are written `#/$defs/…` and `#` is the document root, so a
        // branch lifted out of one names things that are not there unless the
        // definitions come with it.
        let schema = input_schema(RequestKind::TablePreview);
        let text = serde_json::to_string(&schema).expect("it serialises");
        if text.contains("$ref") {
            assert!(schema.contains_key("$defs"), "{text}");
        }
    }

    #[test]
    fn a_schema_carries_no_definition_it_does_not_name() {
        // Every `$ref` a tool names resolves, and nothing else rides along: the
        // protocol document's `$defs` is mostly response types, and a copy of
        // it per tool is a hundred kilobytes of schema an agent reads before
        // it can call anything.
        let names = |schema: &JsonObject| -> Vec<String> {
            schema
                .get("$defs")
                .and_then(Json::as_object)
                .map(|d| d.keys().cloned().collect())
                .unwrap_or_default()
        };
        for kind in RequestKind::ALL {
            if !offered(*kind) {
                continue;
            }
            let schema = input_schema(*kind);
            let mut carried = names(&schema);
            carried.sort();
            let mut named: Vec<String> = refs(&Json::Object(schema));
            named.sort();
            named.dedup();
            assert_eq!(carried, named, "{}", kind.tag());
        }
        // And the one branch that does refer to something still gets it.
        assert_eq!(names(&input_schema(RequestKind::TablePreview)), ["SortBy"]);
        assert!(names(&input_schema(RequestKind::QueryCancel)).is_empty());
    }

    #[test]
    fn a_tool_that_changes_something_says_so() {
        let read_only = |kind| {
            crate::tool(kind)
                .annotations
                .and_then(|a| a.read_only_hint)
                .expect("a hint")
        };
        assert!(read_only(RequestKind::TablePreview));
        assert!(read_only(RequestKind::QueryEstimate));
        // Not `query_run`: what it runs is whatever SQL it was handed.
        assert!(!read_only(RequestKind::QueryRun));
        assert!(!read_only(RequestKind::ConnectionClose));
    }

    #[test]
    fn only_what_cannot_be_taken_back_is_destructive() {
        assert!(is_destructive(RequestKind::ConnectionClose));
        assert!(is_destructive(RequestKind::QueryCancel));
        // Opening one is additive, and running a statement is refused or
        // undone by the database rather than by this flag.
        assert!(!is_destructive(RequestKind::ConnectionOpen));
        assert!(!is_destructive(RequestKind::TablePreview));
    }

    #[test]
    fn every_tool_that_asks_for_a_connection_is_one_the_server_can_fill() {
        // The two lists have to agree: a request whose schema requires a
        // connection but that `takes_a_connection` denies would fail every
        // call that left it out.
        for kind in RequestKind::ALL {
            if !offered(*kind) {
                continue;
            }
            let schema = input_schema(*kind);
            let has_field = schema["properties"]
                .as_object()
                .is_some_and(|p| p.contains_key("connection"));
            assert_eq!(
                has_field,
                takes_a_connection(*kind),
                "{} has a connection field: {has_field}",
                kind.tag()
            );
        }
    }

    #[test]
    fn every_tool_says_what_it_is_for() {
        for kind in RequestKind::ALL {
            let text = describes(*kind);
            assert!(text.len() > 20, "{}: {text}", kind.tag());
            assert!(text.ends_with('.'), "{}: {text}", kind.tag());
        }
    }
}
