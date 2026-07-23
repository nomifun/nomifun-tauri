use std::collections::BTreeSet;

use nomifun_common::AgentExecutionEventKind;
use ts_rs::{Config, TS};

const BASELINE: &str = include_str!("../migrations/001_v3_baseline.sql");
const TYPESCRIPT_BINDING: &str = include_str!(
    "../../../../ui/src/common/protocolBindings/AgentExecutionEventKind.ts"
);

fn quoted_values(source: &str, quote: char) -> BTreeSet<String> {
    let mut values = BTreeSet::new();
    let mut remaining = source;
    while let Some(start) = remaining.find(quote) {
        remaining = &remaining[start + quote.len_utf8()..];
        let end = remaining
            .find(quote)
            .expect("contract literal must have a closing quote");
        values.insert(remaining[..end].to_owned());
        remaining = &remaining[end + quote.len_utf8()..];
    }
    values
}

#[test]
fn rust_sql_and_typescript_share_one_exact_event_vocabulary() {
    let generated_typescript = AgentExecutionEventKind::export_to_string(&Config::default())
        .expect("canonical Rust event enum must generate a TypeScript binding");
    assert_eq!(
        TYPESCRIPT_BINDING, generated_typescript,
        "committed TypeScript binding is stale; regenerate the ts-rs export"
    );

    let sql_start = BASELINE
        .find("event_type            TEXT NOT NULL CHECK (event_type IN (")
        .expect("event_type CHECK must exist in the v3 baseline");
    let sql_check = &BASELINE[sql_start..];
    let sql_end = sql_check
        .find(")),")
        .expect("event_type CHECK must remain bounded");
    let sql_values = quoted_values(&sql_check[..sql_end], '\'');

    let binding_start = generated_typescript
        .find("export type AgentExecutionEventKind =")
        .expect("ts-rs binding must export AgentExecutionEventKind");
    let binding = &generated_typescript[binding_start..];
    let binding_end = binding
        .find(';')
        .expect("generated TypeScript alias must be terminated");
    let typescript_values = quoted_values(&binding[..binding_end], '"');

    assert_eq!(sql_values.len(), 8, "durable event vocabulary changed");
    assert_eq!(typescript_values, sql_values);
    assert!(!sql_values.contains("migrated"));

    for wire in sql_values {
        let kind: AgentExecutionEventKind = wire
            .parse()
            .unwrap_or_else(|error| panic!("SQL event `{wire}` is not a Rust event kind: {error}"));
        assert_eq!(kind.as_str(), wire);
        assert_eq!(serde_json::to_value(kind).unwrap(), wire);
    }
}

#[test]
fn v3_agent_event_actor_contract_is_named_and_local_integer_free() {
    let start = BASELINE
        .find("CREATE TABLE agent_execution_events")
        .expect("agent execution event table must exist");
    let end = BASELINE[start..]
        .find("CREATE TABLE agent_execution_template_participants")
        .map(|offset| start + offset)
        .expect("agent execution event table must remain bounded");
    let event_table = &BASELINE[start..end];

    for required_column in [
        "id                    INTEGER PRIMARY KEY AUTOINCREMENT",
        "execution_id          TEXT NOT NULL",
        "actor_id              TEXT",
        "actor_conversation_id TEXT",
        "actor_attempt_id      TEXT",
    ] {
        assert!(
            event_table.contains(required_column),
            "v3 agent event contract is missing `{required_column}`"
        );
    }
    for retired_numeric_contract in [
        "actor_conversation_id > 0",
        "CAST(actor_conversation_id AS TEXT)",
        "CAST(conversation.id AS TEXT)",
        "FOREIGN KEY",
        "REFERENCES",
    ] {
        assert!(
            !event_table.contains(retired_numeric_contract),
            "retired physical/numeric actor-ID contract survived: {retired_numeric_contract}"
        );
    }
}
