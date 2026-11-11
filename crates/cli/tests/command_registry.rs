use indexmap::IndexMap;
use mnemosyne::{
    args,
    registry::{frame_error, frame_output, CommandRegistry, CommandSpec, FlagSpec},
    search::Params,
};
use serde::Deserialize;
use serde_json::{Map, Value};

#[derive(Deserialize)]
struct Manifest {
    reference_version: String,
    authored_metadata: Map<String, Value>,
    suggestion_background: Vec<String>,
    base_completions: Value,
    shadow_completions: Value,
    cases: Vec<Case>,
}
#[derive(Deserialize)]
struct Case {
    id: String,
    phase: String,
    args: Vec<String>,
    expected: Expected,
}
#[derive(Deserialize)]
struct Expected {
    stdout: String,
    stderr: String,
    exit_code: i32,
}
#[derive(Debug, Clone, PartialEq)]
enum Handler {
    Base,
    Shadow,
    Help,
    Placeholder,
}

fn flags(metadata: &Map<String, Value>) -> IndexMap<String, FlagSpec> {
    metadata
        .iter()
        .map(|(key, value)| {
            (
                key.clone(),
                FlagSpec::from_json(value.as_object().unwrap().clone()),
            )
        })
        .collect()
}
fn registry(manifest: &Manifest) -> CommandRegistry<Handler> {
    let mut registry = CommandRegistry::new();
    for name in &manifest.suggestion_background {
        registry
            .register(name, CommandSpec::new("", None), Handler::Placeholder)
            .unwrap();
    }
    let mut help = IndexMap::new();
    help.insert(
        "<command>".into(),
        FlagSpec::new("Show help for a specific command"),
    );
    registry
        .register(
            "help",
            CommandSpec::new("Show list of all available commands", Some(help)),
            Handler::Help,
        )
        .unwrap();
    registry
        .register(
            "aliasprobe",
            CommandSpec::new(
                "Echo framework arguments",
                Some(flags(&manifest.authored_metadata)),
            ),
            Handler::Base,
        )
        .unwrap();
    registry
}
fn completion_subset(registry: &CommandRegistry<Handler>) -> Value {
    Value::Object(
        registry
            .completions()
            .as_object()
            .unwrap()
            .iter()
            .filter(|(key, _)| key.starts_with("aliasprobe"))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    )
}
fn execute(registry: &CommandRegistry<Handler>, argv: &[String]) -> String {
    let prepared = match registry.prepare(argv) {
        Ok(prepared) => prepared,
        Err(error) => return frame_error(&error),
    };
    let entry = registry.get(&prepared.command).unwrap();
    let response = match entry.handler {
        Handler::Help => registry.help(&prepared.params),
        Handler::Base | Handler::Shadow => {
            let values: Map<_, _> = prepared
                .params
                .into_iter()
                .map(|(key, value)| (key, Value::String(value.unwrap())))
                .collect();
            serde_json::to_string(&serde_json::json!({"handler":if entry.handler == Handler::Base { "base" } else { "shadow" }, "args":values})).unwrap()
        }
        Handler::Placeholder => panic!("suggestion-only background handler was executed"),
    };
    frame_output(&response)
}

#[test]
fn dynamic_framework_matches_52_captured_pipe_cases() {
    let manifest: Manifest =
        serde_json::from_str(include_str!("fixtures/command_registry.json")).unwrap();
    assert_eq!(manifest.reference_version, "1.14.4");
    let mut registry = registry(&manifest);
    assert_eq!(completion_subset(&registry), manifest.base_completions);
    let mut shadow = false;
    let mut failures = Vec::new();
    for case in manifest.cases {
        if case.phase == "shadow_registered" && !shadow {
            registry
                .register(
                    "aliasprobe:done",
                    CommandSpec::new("Exact shadow command", None),
                    Handler::Shadow,
                )
                .unwrap();
            assert_eq!(completion_subset(&registry), manifest.shadow_completions);
            shadow = true;
        } else if case.phase == "shadow_removed" && shadow {
            assert!(registry.unregister("aliasprobe:done").is_some());
            assert_eq!(completion_subset(&registry), manifest.base_completions);
            shadow = false;
        }
        let actual = execute(&registry, &case.args);
        if actual != case.expected.stdout {
            failures.push(format!(
                "{} {:?}\nexpected {:?}\nactual {:?}",
                case.id, case.args, case.expected.stdout, actual
            ));
        }
        assert_eq!(case.expected.stderr, "");
        assert_eq!(case.expected.exit_code, 0);
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn object_enumeration_and_primitive_prototype_setter_match_source_contract() {
    let argv: Vec<_> = [
        "aliasprobe",
        "query=needle",
        "z=first",
        "10=ten",
        "2=two",
        "01=leading",
        "4294967295=ordinary",
        "0=zero",
        "z=last",
        "__proto__=ignored",
    ]
    .map(String::from)
    .into();
    let (_, params) = args::parse_argv(&argv);
    assert_eq!(
        params.keys().map(String::as_str).collect::<Vec<_>>(),
        ["0", "2", "10", "query", "z", "01", "4294967295"]
    );
    assert_eq!(params["z"].as_deref(), Some("last"));
    assert!(!params.contains_key("__proto__"));
    assert!(args::property_present(&params, "__proto__"));
    assert!(args::property_truthy(&params, "toString"));
    let mut registry = CommandRegistry::new();
    let mut flags = IndexMap::new();
    flags.insert("2".into(), FlagSpec::new("Numeric alias"));
    registry
        .register("probe", CommandSpec::new("", Some(flags)), ())
        .unwrap();
    let prepared = registry
        .prepare(&["probe:2".into(), "plain=value".into()])
        .unwrap();
    assert_eq!(
        prepared
            .params
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["2", "plain"]
    );
}

#[test]
fn lifecycle_required_inherited_presence_and_output_boundary() {
    let mut registry = CommandRegistry::new();
    let mut flags = IndexMap::new();
    flags.insert(
        "constructor".into(),
        FlagSpec::new("Inherited presence").required(),
    );
    registry
        .register("probe", CommandSpec::new("Lifecycle", Some(flags)), 7)
        .unwrap();
    assert!(registry
        .register("probe", CommandSpec::new("Duplicate", None), 8)
        .unwrap_err()
        .to_string()
        .contains("already registered"));
    assert!(!registry.unregister_if("probe", |handler| *handler == 8));
    assert!(registry.prepare(&["probe".into()]).is_ok());
    assert!(registry.unregister_if("probe", |handler| *handler == 7));
    assert!(registry.get("probe").is_none());
    assert_eq!(frame_output(""), "");
    assert_eq!(frame_output("last line"), "last line\n");
    assert_eq!(frame_output("x\r\n"), "x\r\n");
    let mut registry = CommandRegistry::new();
    registry
        .register("help", CommandSpec::new("Help", None), ())
        .unwrap();
    assert_eq!(registry.prepare(&[]).unwrap().command, "help");
    assert_eq!(
        registry.prepare(&["--help".into()]).unwrap().command,
        "help"
    );
    let prepared = registry.prepare(&["help".into(), "--copy".into()]).unwrap();
    assert!(prepared.copy_requested);
    assert!(!prepared.params.contains_key("--copy"));
}

#[test]
fn help_numeric_object_keys_take_priority_over_earlier_plain_keys() {
    let mut registry = CommandRegistry::new();
    registry
        .register("2", CommandSpec::new("Numeric command", None), ())
        .unwrap();
    registry
        .register("plain", CommandSpec::new("Plain command", None), ())
        .unwrap();
    let mut params = Params::new();
    params.insert("plain".into(), Some("true".into()));
    params.insert("2".into(), Some("true".into()));
    assert_eq!(
        registry.help(&params),
        "  2                     Numeric command\n"
    );
}
