//! A tool call, from the name a client sends to the answer it reads.
//!
//! Outside `src` because it is about the pair: the tool list and the protocol
//! have to agree, and neither can prove that on its own. It runs against a
//! store in this process, which is what `Backend::Local` is.

use std::sync::Arc;

use rmcp::model::CallToolRequestParams;
use serde_json::{Map, Value as Json, json};
use sqlake_api::{Backend, Response, Service};
use sqlake_app::action::Action;
use sqlake_app::store::{Drivers, Store, Wiring};
use sqlake_core::id::{ConnId, ProfileId};
use sqlake_driver_mock::{Behaviour, MockDriver, MockProfiles};
use sqlake_mcp::Server;

async fn server() -> Server {
    let store = Store::spawn(
        Wiring::new(
            Drivers::new().with(Arc::new(MockDriver::new(Behaviour::instant()))),
            Arc::new(MockProfiles::default()),
        )
        .page_size(50),
    );
    let conn = ConnId::new();
    store
        .dispatch_and_settle(
            Action::Connect {
                profile: ProfileId::parse("mock").expect("a usable id"),
                conn,
            },
            sqlake_api::DEFAULT_TIMEOUT,
            |s| s.connection_settled(conn),
        )
        .await
        .expect("it connects");
    Server::new(Backend::Local(Box::new(Service::new(store))), None)
}

/// A session with a template saved and nothing connected.
async fn server_with_a_template() -> Server {
    use sqlake_core::library::{Library as _, NewTemplate};

    let library = sqlake_library::Sqlite::in_memory().expect("a library opens");
    library
        .add(NewTemplate {
            name: "daily".to_owned(),
            body: "select * from {{ident:table}}".to_owned(),
            driver: None,
            tags: Vec::new(),
        })
        .expect("it saves");
    let store = Store::spawn(
        Wiring::new(
            Drivers::new().with(Arc::new(MockDriver::new(Behaviour::instant()))),
            Arc::new(MockProfiles::default()),
        )
        .library(Arc::new(library)),
    );
    Server::new(Backend::Local(Box::new(Service::new(store))), None)
}

fn call(name: &str, arguments: Json) -> CallToolRequestParams {
    let Json::Object(arguments) = arguments else {
        panic!("arguments are an object");
    };
    let mut params = CallToolRequestParams::default();
    params.name = name.to_owned().into();
    params.arguments = Some(arguments);
    params
}

#[tokio::test]
async fn a_tool_call_is_the_request_it_names() {
    let server = server().await;
    let answer = server.answer(call("connection_list", json!({}))).await;
    let Response::Connections(open) = answer else {
        panic!("{answer:?}");
    };
    assert_eq!(open.len(), 1);
}

#[tokio::test]
async fn a_tool_that_names_no_connection_gets_the_sessions() {
    // The whole reason `choose` moved down: a caller that had to list
    // connections before every call would spend half its turns on bookkeeping.
    let server = server().await;
    let answer = server
        .answer(call("table_list", json!({ "namespace": ["public"] })))
        .await;
    let Response::Nodes(tables) = answer else {
        panic!("{answer:?}");
    };
    assert!(!tables.is_empty());
}

#[tokio::test]
async fn a_tool_that_names_one_uses_it() {
    let server = server().await;
    let Response::Connections(open) = server.answer(call("connection_list", json!({}))).await
    else {
        panic!("should have listed them");
    };
    let answer = server
        .answer(call(
            "table_preview",
            json!({ "connection": open[0].id, "table": ["public", "users"] }),
        ))
        .await;
    assert!(matches!(answer, Response::Page(_)), "{answer:?}");
}

#[tokio::test]
async fn a_connection_that_is_not_there_is_the_surfaces_own_failure() {
    // Answered rather than refused at this layer: the id is well formed and
    // the tool exists, so what went wrong is something the session knows.
    let server = server().await;
    let answer = server
        .answer(call(
            "table_list",
            json!({ "connection": "nope", "namespace": ["public"] }),
        ))
        .await;
    assert!(
        matches!(
            answer,
            Response::Failed(sqlake_api::Failure::NoSuchConnection { .. })
        ),
        "{answer:?}"
    );
}

#[tokio::test]
async fn a_tool_nobody_offers_is_refused_before_anything_is_sent() {
    let server = server().await;
    let answer = server.answer(call("drop_everything", json!({}))).await;
    let Response::Failed(sqlake_api::Failure::Malformed { message }) = answer else {
        panic!("{answer:?}");
    };
    assert!(message.contains("drop_everything"), "{message}");
}

#[tokio::test]
async fn an_argument_the_request_does_not_have_is_refused() {
    // `deny_unknown_fields` on the protocol reaches a tool call, because a
    // tool call *is* deserialised as one — which is the point of building the
    // request rather than a parallel argument struct.
    let server = server().await;
    let answer = server
        .answer(call(
            "table_list",
            json!({ "namespace": ["public"], "sneaky": true }),
        ))
        .await;
    assert!(
        matches!(
            answer,
            Response::Failed(sqlake_api::Failure::Malformed { .. })
        ),
        "{answer:?}"
    );
}

#[tokio::test]
async fn the_tag_a_caller_writes_is_ignored_rather_than_obeyed() {
    // The tool's name is the request's tag. A caller that wrote a different
    // one — by accident or otherwise — must not reach a different request.
    let server = server().await;
    let answer = server
        .answer(call(
            "connection_list",
            json!({ "request": "connection_close", "connection": "c" }),
        ))
        .await;
    // Refused rather than run as either one: `connection_list` takes no
    // fields, so the extra ones fail `deny_unknown_fields` — and the tag it
    // tried to smuggle was overwritten before that.
    assert!(
        matches!(
            answer,
            Response::Failed(sqlake_api::Failure::Malformed { .. })
        ),
        "{answer:?}"
    );
}

#[tokio::test]
async fn every_tool_the_list_offers_can_be_called() {
    // Not that each succeeds — most need something this fixture has not got —
    // but that none is refused as unknown, which is what a name in the list
    // that the dispatcher does not recognise would look like.
    let server = server().await;
    for tool in sqlake_mcp::tools::all() {
        let answer = server
            .answer(call(tool.name.as_ref(), Json::Object(Map::new())))
            .await;
        if let Response::Failed(sqlake_api::Failure::Malformed { message }) = &answer {
            assert!(
                !message.contains("no tool called"),
                "{} is listed and not callable",
                tool.name
            );
        }
    }
}

#[tokio::test]
async fn a_template_applies_with_nothing_connected() {
    // The server fills a connection in wherever it can, and this is the one
    // tool that has an answer when it cannot: filling a template in needs
    // quoting rules, not a database. Refusing here made this wrapper stricter
    // than the protocol it wraps.
    let server = server_with_a_template().await;
    let answer = server
        .answer(call(
            "template_apply",
            json!({"template": "daily", "values": {"table": "users"}}),
        ))
        .await;
    let Response::Statement(statement) = answer else {
        panic!("{answer:?}");
    };
    assert_eq!(statement.sql, r#"select * from "users""#);
}

#[tokio::test]
async fn a_tool_that_needs_a_connection_still_says_when_there_is_none() {
    // The other half: only the optional ones carry on without one.
    let server = server_with_a_template().await;
    let answer = server.answer(call("namespace_list", json!({}))).await;
    assert!(matches!(answer, Response::Failed(_)), "{answer:?}");
}
