//! Authored nonvisual state/lexer controls. Native stream replay is separate.
use mnemosyne_engine::interactive::{lex, Command, InputOutcome, InteractiveSession};
fn u(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}
fn input(session: &mut InteractiveSession, bytes: &[u8]) -> InputOutcome {
    session.input(bytes, |_| vec![]).unwrap()
}
fn execute(session: &mut InteractiveSession, line: &str) -> Command {
    assert_eq!(input(session, line.as_bytes()), InputOutcome::Continue);
    let InputOutcome::Execute(command) = input(session, b"\r") else {
        panic!("expected command");
    };
    command
}
#[test]
fn lexical_quotes_empty_tokens_escapes_and_unmatched_quotes_preserve_units() {
    assert_eq!(
        lex(&u("create path=\"A B.md\" content='one \\'two' '' tail\\")),
        vec![
            u("create"),
            u("path=A B.md"),
            u("content=one 'two"),
            u("tail")
        ]
    );
    assert_eq!(
        lex(&u("a\tb\u{a0}c\n' d'")),
        vec![u("a"), u("b\u{a0}c\n d")]
    );
    assert_eq!(
        lex(&[b'x' as u16, 0xd800, b' ' as u16, 0xdc00]),
        vec![vec![b'x' as u16, 0xd800], vec![0xdc00]]
    );
}
#[test]
fn execution_and_vault_selection_change_only_their_declared_state() {
    let mut session = InteractiveSession::new(Some("a".into()), u("Alpha"));
    assert_eq!(
        execute(&mut session, "vault=Beta read path='A B.md'"),
        Command::Execute {
            vault_selector: Some(u("Beta")),
            argv: vec![u("read"), u("path=A B.md")]
        }
    );
    assert_eq!(session.current_vault_id.as_deref(), Some("a"));
    assert!(session.is_processing);
    input(&mut session, b"discard this while processing");
    assert!(!session.input_buffer.ends_with(&u("processing")));
    session.finish_command(|_| vec![]);
    assert_eq!(
        execute(&mut session, "vault:open Beta"),
        Command::OpenVault { name: u("Beta") }
    );
    session.select_vault("b".into(), u("Beta"));
    session.finish_command(|_| vec![]);
    assert_eq!(session.current_vault_id.as_deref(), Some("b"));
    assert!(session.input_buffer.is_empty());
    assert_eq!(session.history.len(), 2);
}
#[test]
fn data_chunk_and_utf16_edit_boundaries_are_not_silently_improved() {
    let mut session = InteractiveSession::default();
    assert_eq!(input(&mut session, b"version\r"), InputOutcome::Continue);
    assert_eq!(session.input_buffer, u("version\r"));
    assert_eq!(
        input(&mut session, b"\r"),
        InputOutcome::Execute(Command::Execute {
            vault_selector: None,
            argv: vec![u("version")]
        })
    );
    session.finish_command(|_| vec![]);
    input(&mut session, "😀".as_bytes());
    input(&mut session, b"\x7f");
    assert_eq!(session.input_buffer, vec![0xd83d]);
    assert_eq!(session.cursor, 1);
    input(&mut session, b"\x01");
    input(&mut session, b"x");
    assert_eq!(session.input_buffer, vec![b'x' as u16, 0xd83d]);
}
#[test]
fn edits_history_search_and_repeated_commands_use_retained_state() {
    let mut session = InteractiveSession::default();
    execute(&mut session, "read path=First.md");
    session.finish_command(|_| vec![]);
    execute(&mut session, "read path=Second.md");
    session.finish_command(|_| vec![]);
    input(&mut session, b"\x1b[A");
    assert_eq!(session.input_buffer, u("read path=Second.md"));
    input(&mut session, b"\x1b[A");
    assert_eq!(session.input_buffer, u("read path=First.md"));
    input(&mut session, b"\x1b[B");
    assert_eq!(session.input_buffer, u("read path=Second.md"));
    input(&mut session, b"\x1b[B");
    assert!(session.input_buffer.is_empty());
    input(&mut session, b"\x12");
    for ch in b"FIRST" {
        input(&mut session, &[*ch]);
    }
    assert_eq!(session.search_matches, vec![u("read path=First.md")]);
    input(&mut session, b"\r");
    assert_eq!(session.input_buffer, u("read path=First.md "));
    let InputOutcome::Execute(_) = input(&mut session, b"\r") else {
        panic!()
    };
    session.finish_command(|_| vec![]);
    assert_eq!(session.history[0], u("read path=First.md"));
    execute(&mut session, "read path=First.md");
    session.finish_command(|_| vec![]);
    assert_eq!(session.history.len(), 3);
}
#[test]
fn supplied_suggestions_accept_in_two_steps_and_escape_restores_input() {
    let mut session = InteractiveSession::default();
    let provider = |_: &[u16]| vec![u("read")];
    session.input(b"r", provider).unwrap();
    session.input(b"\t", provider).unwrap();
    assert_eq!(session.suggestion_index, Some(0));
    assert_eq!(
        session.input(b"\r", provider).unwrap(),
        InputOutcome::Continue
    );
    assert_eq!(session.input_buffer, u("read "));
    session.input(b"\x1b", provider).unwrap();
    assert_eq!(session.input_buffer, u("r"));
}
#[test]
fn quit_exit_and_control_exit_do_not_execute_a_backend_command() {
    for bytes in [b"\x03".as_slice(), b"\x04".as_slice()] {
        assert_eq!(
            input(&mut InteractiveSession::default(), bytes),
            InputOutcome::Exit
        );
    }
    for word in ["exit", "quit"] {
        let mut session = InteractiveSession::default();
        input(&mut session, word.as_bytes());
        assert_eq!(input(&mut session, b"\r"), InputOutcome::Exit);
        assert!(session.history.is_empty());
    }
}

fn catalog() -> mnemosyne_engine::interactive::CompletionCatalog {
    use serde_json::json;
    mnemosyne_engine::interactive::CompletionCatalog::new(
        json!({
            "search":{"description":"Read matching content", "flags":{
                "path":{"value":"<path>","description":"A file"},
                "folder":{"value":"<folder>","description":"A folder"},
                "format":{"value":"text|json","description":"Output format"},
                "case":{"description":"Case sensitive"}}},
            "read":{"description":"Read a note","flags":{"file":{"value":"<file>","description":"A note"}}},
            "base:read":{"description":"Read a base","flags":{}},
            "command":{"flags":{"id":{"value":"<command-id>","description":"Action"}}},
            "legacy":{"usage":"[path=<path>] [total] [limit=<n>]"}
        }),
        vec![u("Alpha Vault"), u("Beta")],
        vec![
            u("Sub/Alpha.md"),
            u("Sub/Nested/Beta.MD"),
            u("Sub/List.base"),
            u("Root.md"),
            u("Sub/Other.md"),
        ],
        vec![
            (u("action:first"), u("First action")),
            (u("other"), u("Read something")),
        ],
    )
}
#[test]
fn command_completions_sort_utf16_prefix_then_description_and_keep_descriptions() {
    let c = catalog();
    assert_eq!(
        c.suggestions(&u("rea")),
        vec![u("read"), u("base:read"), u("search")]
    );
    let details = c.suggestion_details(&u("EXIT")).unwrap();
    assert_eq!(details[0].text, u("exit"));
    assert_eq!(details[0].description, Some(u("Exit the CLI")));
    assert_eq!(
        c.suggestions(&u("vault:open a")),
        vec![u("vault:open \"Alpha Vault\"")]
    );
}
#[test]
fn flag_enum_completions_use_case_sensitive_names_and_do_not_repeat_used_flags() {
    let c = catalog();
    assert_eq!(
        c.suggestions(&u("search ")),
        vec![
            u("search path="),
            u("search folder="),
            u("search format="),
            u("search case")
        ]
    );
    assert_eq!(
        c.suggestions(&u("search path=A case ")),
        vec![
            u("search path=A case folder="),
            u("search path=A case format=")
        ]
    );
    assert!(c.suggestions(&u("search PA")).is_empty());
    let details = c.suggestion_details(&u("search format=J")).unwrap();
    assert_eq!(details[0].text, u("search format=json"));
    assert_eq!(details[0].description, Some(u("Output format")));
    assert_eq!(
        c.suggestions(&u("legacy ")),
        vec![u("legacy path="), u("legacy total"), u("legacy limit=")]
    );
}
#[test]
fn file_folder_action_completions_preserve_catalog_order_and_native_quote_quirks() {
    let c = catalog();
    assert_eq!(
        c.suggestions(&u("read file=SUB")),
        vec![
            u("read file=\"Sub/Alpha\""),
            u("read file=\"Sub/Nested/Beta.MD\""),
            u("read file=\"Sub/List.base\""),
            u("read file=\"Sub/Other\"")
        ]
    );
    assert_eq!(
        c.suggestions(&u("base:read path=")),
        vec![u("base:read path=\"Sub/List.base\"")]
    );
    assert_eq!(
        c.suggestions(&u("search folder=")),
        vec![
            u("search folder=\"Sub\""),
            u("search folder=\"Sub/Nested\"")
        ]
    );
    assert_eq!(
        c.suggestions(&u("command id=read")),
        vec![u("command id=other")]
    );
    assert!(c.suggestions(&u("read file=\"Sub")).is_empty()); // Completion splitter is not Lo lexer.
    assert_eq!(c.suggestions(&u("vault:open Alpha ")),vec![u("vault:open \"Alpha Vault\""),u("vault:open Beta")]);
    // Only literal final ASCII space requests the next argument; final tab does not.
    assert_eq!(
        c.suggestions(&u("read\t")),
        vec![u("read"), u("base:read"), u("search")]
    );
}
#[test]
fn completion_limit_and_unicode_casing_are_shared_with_the_pinned_backend() {
    use mnemosyne_engine::interactive::CompletionCatalog;
    let files = (0..25).map(|n| u(&format!("File{n}.md"))).collect();
    let c = CompletionCatalog::new(
        serde_json::json!({"Σ":{"description":"Greek"}}),
        vec![],
        files,
        vec![],
    );
    assert_eq!(c.suggestions(&u("σ")), vec![u("Σ")]);
    let matches = c.suggestions(&u("unknown path=file"));
    assert_eq!(matches.len(), 20);
    assert_eq!(matches[0], u("unknown path=\"File0.md\""));
    assert_eq!(matches[19], u("unknown path=\"File19.md\""));
    assert_eq!(c.suggestions(&[0xd800]), Vec::<Vec<u16>>::new());
}
