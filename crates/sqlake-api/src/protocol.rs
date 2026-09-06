//! What crosses the socket, and the schema that describes it.
//!
//! The schema is generated from these types by `schemars` rather than written
//! alongside them. A hand-written one is a second description of the protocol
//! that nothing checks, and it drifts in the direction that matters: it goes on
//! describing a call after the call has changed. Generated, it can describe no
//! request that does not exist — which is the property an agent reading it
//! depends on, since the schema is how it learns the surface instead of having
//! it pasted into a prompt.

use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as Json};

use crate::page::{Budget, Page};
use crate::snapshot::{ConnectionInfo, NodeInfo, SessionInfo};

/// Which column to sort a preview by.
///
/// An index rather than a name because that is what the store's `SortPreview`
/// takes, and inventing a name here would mean resolving it against a set of
/// columns the caller has already been sent. A column that is not there is
/// refused by the store rather than retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
pub struct SortBy {
    /// Index into the `columns` of the page this sorts.
    pub column: usize,
}

/// What a caller can ask for.
///
/// Internally tagged, so a request is one flat object: these are written by
/// callers, sometimes by hand, and `{"request": "table_list", "connection":
/// "…"}` is easier to get right than a nested `params`. [`Response`] is tagged
/// the other way for the opposite reason — see there.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "request", deny_unknown_fields)]
pub enum Request {
    /// The session as a whole: its connections and the profiles it could open.
    ///
    /// A struct variant with no fields rather than a unit one: serde reads a
    /// unit variant out of an internally tagged enum with a visitor that
    /// accepts any map, so `deny_unknown_fields` never reaches it and
    /// `{"request": "snapshot", "connection": "…"}` would be answered as a
    /// plain snapshot. The wire form and the generated schema are the same
    /// either way.
    Snapshot {},

    /// This document.
    ///
    /// A request rather than only a subcommand so that a caller which reached
    /// the socket first does not have to shell out to discover what else it can
    /// send.
    Schema {},

    /// Every connection currently open.
    ConnectionList {},

    /// The namespaces in a connection.
    ///
    /// Named for the structural level rather than the human one: a driver calls
    /// this a schema or a dataset, and `Capabilities::hierarchy` is what says
    /// which. Putting the label in the request name would make the wire format
    /// disagree with one of the two drivers.
    NamespaceList { connection: String },

    /// The relations in one namespace.
    TableList {
        connection: String,
        /// The namespace's path, whole. A namespace containing a dot is a real
        /// thing, and rejoining `public.my.schema` from pieces is where that
        /// goes wrong.
        namespace: Vec<String>,
    },

    /// A page of one relation.
    TablePreview {
        connection: String,
        table: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sort: Option<SortBy>,
        /// Fewer rows than the server's budget allows.
        ///
        /// Only ever downward: the budget exists to stop a caller filling its
        /// own context with a table it has not read, and a request that could
        /// raise it would be that protection asking permission from the party
        /// it protects against.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
    },
}

/// Generates the kind enum and its complete list together, so the two cannot
/// drift apart.
macro_rules! request_kinds {
    ($($name:ident => $wire:literal),* $(,)?) => {
        /// One request, named. Exists so "every request appears in the schema"
        /// can be asserted over a list rather than a reviewer's memory.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum RequestKind {
            $($name),*
        }

        impl RequestKind {
            pub const ALL: &'static [Self] = &[$(Self::$name),*];

            /// The tag this appears under on the wire.
            #[must_use]
            pub const fn tag(self) -> &'static str {
                match self { $(Self::$name => $wire),* }
            }
        }
    };
}

request_kinds! {
    Snapshot       => "snapshot",
    Schema         => "schema",
    ConnectionList => "connection_list",
    NamespaceList  => "namespace_list",
    TableList      => "table_list",
    TablePreview   => "table_preview",
}

impl Request {
    /// Exhaustive on purpose.
    ///
    /// Adding a variant stops this compiling, which forces the new request into
    /// [`RequestKind::ALL`] — and a kind the generated schema does not describe
    /// fails `the_schema_describes_every_request_and_no_others`. That chain is
    /// what makes the schema's completeness a fact rather than a claim.
    #[must_use]
    pub const fn kind(&self) -> RequestKind {
        match self {
            Self::Snapshot {} => RequestKind::Snapshot,
            Self::Schema {} => RequestKind::Schema,
            Self::ConnectionList {} => RequestKind::ConnectionList,
            Self::NamespaceList { .. } => RequestKind::NamespaceList,
            Self::TableList { .. } => RequestKind::TableList,
            Self::TablePreview { .. } => RequestKind::TablePreview,
        }
    }

    /// The budget this request asks for, which can only be smaller than the
    /// server's.
    #[must_use]
    pub fn budget(&self, server: Budget) -> Budget {
        match self {
            Self::TablePreview {
                limit: Some(rows), ..
            } => Budget {
                max_rows: server.max_rows.min(*rows),
                ..server
            },
            _ => server,
        }
    }
}

/// Why a request could not be answered.
///
/// A response rather than a transport failure: the socket delivered it, and a
/// caller that has to distinguish "the table is not there" from "the connection
/// dropped" gets a code to branch on rather than a sentence to match against.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "error")]
pub enum Failure {
    /// No connection with this id is open.
    NoSuchConnection { connection: String },
    /// The node or relation is not in the tree the connection loaded.
    NotFound { path: Vec<String> },
    /// The driver refused, or failed.
    Driver { message: String },
    /// The wait for the store to settle ran out.
    Timeout { waited_ms: u64 },
    /// The request was well-formed but asks for something this session cannot
    /// do — a sort on a driver whose preview cannot sort, say.
    Unsupported { message: String },
    /// The line was not a request this build understands.
    ///
    /// Answered rather than dropped: a client whose request could not be parsed
    /// would otherwise watch the socket go quiet and have to guess whether the
    /// session had died.
    Malformed { message: String },
}

/// What a caller is sent back.
///
/// Adjacently tagged, unlike [`Request`]: a response's payload is sometimes an
/// array and sometimes an object, and an internal tag cannot be put on an
/// array. Responses are read by a parser rather than written by hand, so the
/// extra nesting costs nothing here.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "response", content = "data")]
pub enum Response {
    Snapshot(SessionInfo),
    Schema(Json),
    Connections(Vec<ConnectionInfo>),
    /// The answer to both `namespace_list` and `table_list`: one level of the
    /// tree, whichever level it is. A driver's hierarchy is data, so a request
    /// per level would be a promise about depth that `Capabilities::hierarchy`
    /// deliberately does not make.
    Nodes(Vec<NodeInfo>),
    Page(Page),
    Failed(Failure),
}

/// Generates the response kinds and their list together, for the same reason
/// [`RequestKind`] exists: a check over "every response" has to be able to name
/// them all.
macro_rules! response_kinds {
    ($($name:ident),* $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum ResponseKind {
            $($name),*
        }

        impl ResponseKind {
            pub const ALL: &'static [Self] = &[$(Self::$name),*];
        }
    };
}

response_kinds!(Snapshot, Schema, Connections, Nodes, Page, Failed);

impl Response {
    /// Exhaustive on purpose, so a new response cannot be added without the
    /// check on what crosses the socket being told about it.
    #[must_use]
    pub const fn kind(&self) -> ResponseKind {
        match self {
            Self::Snapshot(_) => ResponseKind::Snapshot,
            Self::Schema(_) => ResponseKind::Schema,
            Self::Connections(_) => ResponseKind::Connections,
            Self::Nodes(_) => ResponseKind::Nodes,
            Self::Page(_) => ResponseKind::Page,
            Self::Failed(_) => ResponseKind::Failed,
        }
    }
}

/// The request and response schema, generated from the types.
///
/// Both under one root because a caller wants the pair: a schema for requests
/// alone would tell it what to send and nothing about what comes back.
#[must_use]
pub fn schema() -> Json {
    let mut definitions = Map::new();
    let request = subschema(schema_for!(Request), &mut definitions);
    let response = subschema(schema_for!(Response), &mut definitions);

    let mut root = Map::new();
    root.insert("$schema".into(), Json::from(SCHEMA_DIALECT));
    root.insert("title".into(), Json::from("sqlake agent surface"));
    root.insert("request".into(), request);
    root.insert("response".into(), response);
    root.insert("$defs".into(), Json::Object(definitions));
    Json::Object(root)
}

/// Strips what only belongs to a document root, and hoists what has to live
/// there.
///
/// The dialect, because `schema_for!` stamps every schema it generates as a
/// document in its own right, and two of them nested under one root would each
/// redeclare it — which a validator is entitled to read as a subschema changing
/// dialect mid-document.
///
/// `$defs`, because the `$ref`s pointing at them are written `#/$defs/…` and
/// `#` is the *document* root. Left nested, every reference in both schemas
/// names something that is not there.
fn subschema(schema: schemars::Schema, definitions: &mut Map<String, Json>) -> Json {
    let mut json = serde_json::to_value(schema).expect("a schema serialises");
    let Some(object) = json.as_object_mut() else {
        return json;
    };
    object.remove("$schema");
    if let Some(Json::Object(own)) = object.remove("$defs") {
        for (name, definition) in own {
            let clash = definitions.insert(name.clone(), definition.clone());
            assert!(
                clash.is_none_or(|existing| existing == definition),
                "two different types are both called `{name}`, and hoisting merged them"
            );
        }
    }
    json
}

const SCHEMA_DIALECT: &str = "https://json-schema.org/draft/2020-12/schema";

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::json;

    use super::*;

    /// Every `const` in [`RequestKind::tag`] spelled the way serde spells it.
    ///
    /// The macro cannot check itself: it is handed the wire name as a literal,
    /// so a typo there would name a request the schema does not contain and the
    /// coverage test below would blame the schema.
    #[test]
    fn every_kind_is_tagged_the_way_serde_tags_it() {
        let requests = [
            Request::Snapshot {},
            Request::Schema {},
            Request::ConnectionList {},
            Request::NamespaceList {
                connection: "c".into(),
            },
            Request::TableList {
                connection: "c".into(),
                namespace: vec!["public".into()],
            },
            Request::TablePreview {
                connection: "c".into(),
                table: vec!["public".into(), "users".into()],
                sort: None,
                limit: None,
            },
        ];
        for request in &requests {
            let json = serde_json::to_value(request).expect("a request serialises");
            assert_eq!(
                json["request"],
                Json::from(request.kind().tag()),
                "{request:?} is tagged differently from what its kind claims"
            );
        }
        // The claim that makes `Snapshot {}` free: an empty struct variant is
        // on the wire exactly what the unit variant was, so only the
        // deserializer's behaviour changed.
        assert_eq!(
            serde_json::to_value(Request::Snapshot {}).expect("it serialises"),
            json!({"request": "snapshot"})
        );
        let kinds: BTreeSet<_> = requests.iter().map(Request::kind).collect();
        assert_eq!(
            kinds.len(),
            RequestKind::ALL.len(),
            "a request kind has no sample here, so its tag is unchecked"
        );
    }

    /// Set equality, not containment, so it fails in both directions: a
    /// request the schema does not describe, and a schema describing a call
    /// that cannot be made. Reading the tags out of `oneOf` rather than
    /// searching the text also means a tag appearing only inside somebody's
    /// doc comment does not count as coverage.
    #[test]
    fn the_schema_describes_every_request_and_no_others() {
        let schema = schema();
        let variants = schema["request"]["oneOf"]
            .as_array()
            .expect("the request schema is a union of its variants");
        let described: BTreeSet<String> = variants
            .iter()
            .filter_map(|v| v["properties"]["request"]["const"].as_str())
            .map(str::to_owned)
            .collect();
        let known: BTreeSet<String> = RequestKind::ALL
            .iter()
            .map(|k| k.tag().to_owned())
            .collect();
        assert_eq!(described, known);
    }

    #[test]
    fn the_dialect_is_declared_once() {
        let schema = schema();
        assert_eq!(schema["$schema"], Json::from(SCHEMA_DIALECT));
        assert!(schema["request"].get("$schema").is_none());
        assert!(schema["response"].get("$schema").is_none());
    }

    /// A `$ref` is resolved against the document root, so a definition left
    /// where `schema_for!` put it — under `request` or `response` — is named by
    /// a reference to nothing, and a caller that validates against this
    /// document cannot load it at all.
    #[test]
    fn every_reference_resolves_against_the_root() {
        fn references(json: &Json, found: &mut Vec<String>) {
            match json {
                Json::Object(members) => {
                    for (key, value) in members {
                        match (key.as_str(), value.as_str()) {
                            ("$ref", Some(target)) => found.push(target.to_owned()),
                            _ => references(value, found),
                        }
                    }
                }
                Json::Array(items) => items.iter().for_each(|item| references(item, found)),
                _ => {}
            }
        }

        let schema = schema();
        let definitions = schema["$defs"]
            .as_object()
            .expect("the definitions are hoisted to the root");
        let mut found = Vec::new();
        references(&schema, &mut found);
        assert!(!found.is_empty(), "nothing referenced, so nothing checked");
        for target in found {
            let name = target
                .strip_prefix("#/$defs/")
                .unwrap_or_else(|| panic!("`{target}` is not rooted at the document"));
            assert!(
                definitions.contains_key(name),
                "`{target}` names no definition"
            );
        }
    }

    /// The tag is the whole request for these, so anything beside it is a
    /// caller asking for something that will not happen.
    #[test]
    fn a_request_with_no_fields_still_refuses_one() {
        assert_eq!(
            serde_json::from_value::<Request>(json!({"request": "snapshot"}))
                .expect("the bare request parses"),
            Request::Snapshot {}
        );
        assert!(
            serde_json::from_value::<Request>(
                json!({"request": "connection_list", "connection": "c"})
            )
            .is_err(),
            "a filter that does not exist was accepted and ignored"
        );
    }

    /// The reason the schema is generated rather than written: a caller reads
    /// it instead of having the surface pasted into its prompt, and the text
    /// that makes it usable is the doc comment on the type.
    #[test]
    fn a_request_carries_its_own_explanation() {
        let schema = schema();
        let described = schema["request"]["oneOf"]
            .as_array()
            .expect("a union of variants")
            .iter()
            .filter(|v| v["description"].as_str().is_some_and(|d| !d.is_empty()))
            .count();
        assert_eq!(described, RequestKind::ALL.len());
    }

    #[test]
    fn a_request_round_trips() {
        let request = Request::TablePreview {
            connection: "c".into(),
            table: vec!["public".into(), "users".into()],
            sort: Some(SortBy { column: 2 }),
            limit: Some(5),
        };
        let json = serde_json::to_value(&request).expect("it serialises");
        assert_eq!(
            json,
            json!({
                "request": "table_preview",
                "connection": "c",
                "table": ["public", "users"],
                "sort": {"column": 2},
                "limit": 5,
            })
        );
        assert_eq!(
            serde_json::from_value::<Request>(json).expect("it parses"),
            request
        );
    }

    #[test]
    fn the_optional_halves_of_a_preview_may_be_left_out() {
        let request: Request =
            serde_json::from_value(json!({"request": "table_preview", "connection": "c",
                                          "table": ["public", "users"]}))
            .expect("a preview with no sort and no limit parses");
        assert_eq!(request.budget(Budget::DEFAULT), Budget::DEFAULT);
    }

    #[test]
    fn a_misspelled_field_is_refused_rather_than_ignored() {
        // Without `deny_unknown_fields` a caller that sends `"limit"` as
        // `"rows"` gets the default budget and no indication that the thing it
        // asked for did not happen.
        let bad = json!({"request": "table_preview", "connection": "c",
                         "table": ["public", "users"], "rows": 5});
        assert!(serde_json::from_value::<Request>(bad).is_err());
    }

    #[test]
    fn a_limit_can_only_lower_the_budget() {
        let server = Budget {
            max_rows: 50,
            max_columns: 40,
        };
        let smaller = Request::TablePreview {
            connection: "c".into(),
            table: vec!["t".into()],
            sort: None,
            limit: Some(5),
        };
        assert_eq!(smaller.budget(server).max_rows, 5);

        let greedy = Request::TablePreview {
            connection: "c".into(),
            table: vec!["t".into()],
            sort: None,
            limit: Some(5_000),
        };
        assert_eq!(
            greedy.budget(server).max_rows,
            50,
            "a request raised the budget it is protected by"
        );
        assert_eq!(greedy.budget(server).max_columns, server.max_columns);
    }

    #[test]
    fn a_failure_is_a_response_with_a_code_to_branch_on() {
        let response = Response::Failed(Failure::NoSuchConnection {
            connection: "gone".into(),
        });
        assert_eq!(
            serde_json::to_value(&response).expect("it serialises"),
            json!({
                "response": "failed",
                "data": {"error": "no_such_connection", "connection": "gone"},
            })
        );
    }

    #[test]
    fn a_page_response_carries_the_rows_as_an_array() {
        // The reason `Response` is adjacently tagged: an internal tag cannot be
        // written onto a JSON array, and `Nodes` is one.
        let response = Response::Nodes(Vec::new());
        assert_eq!(
            serde_json::to_value(&response).expect("it serialises"),
            json!({"response": "nodes", "data": []})
        );
    }
}
