//! Runs the shared corpus in `tests/fixtures/permissions` (schema in its
//! README). `RIFT_BLESS_FIXTURES=1 cargo test -p rift-permissions --test
//! fixtures` rewrites the `compiled` member of resolver fixtures from the
//! current compiler; review the diff before committing it.

use rift_permissions::{
    CompiledSubject, ContextSet, Error, Group, Node, Tristate, User, Uuid,
    change::{Applied, Change, ChangeEvent},
    check::{MetaView, Trace},
    compile, luckperms,
    wire::{self, Reply},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{collections::BTreeMap, fs, path::Path};

const REVISION: u64 = 1;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Resolve {
    #[serde(rename = "kind")]
    _kind: String,
    #[serde(rename = "name")]
    _name: String,
    #[serde(rename = "description", default)]
    _description: String,
    now: u64,
    users: BTreeMap<Uuid, FixtureUser>,
    #[serde(default)]
    groups: BTreeMap<String, FixtureGroup>,
    #[serde(default)]
    checks: Vec<Check>,
    #[serde(default)]
    meta: Vec<MetaCheck>,
    compiled: Option<BTreeMap<Uuid, CompiledSubject>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureUser {
    #[serde(default)]
    primary_group: Option<String>,
    #[serde(default)]
    nodes: Vec<Node>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureGroup {
    #[serde(default)]
    weight: i32,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    nodes: Vec<Node>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Check {
    user: Uuid,
    node: String,
    #[serde(default)]
    contexts: ContextSet,
    now: Option<u64>,
    expect: Tristate,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MetaCheck {
    user: Uuid,
    #[serde(default)]
    contexts: ContextSet,
    now: Option<u64>,
    expect: MetaView,
}

fn resolve(path: &Path, text: &str, bless: bool) -> Result<(), String> {
    let fixture: Resolve = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let mut groups = BTreeMap::new();
    for (name, data) in fixture.groups {
        let mut group = Group::new(&name).map_err(|e| e.to_string())?;
        group.weight = data.weight;
        group.display_name = data.display_name;
        group.nodes = data.nodes;
        groups.insert(group.name.clone(), group);
    }
    let mut compiled = BTreeMap::new();
    for (uuid, data) in fixture.users {
        let user = User {
            uuid,
            last_name: None,
            primary_group: data.primary_group,
            nodes: data.nodes,
        };
        let subject = compile(&user, &groups, fixture.now, REVISION).map_err(|e| e.to_string())?;
        compiled.insert(uuid, subject);
    }
    if bless {
        fs::write(path, blessed(text, &compiled)).map_err(|e| e.to_string())?;
    } else if fixture.compiled.as_ref() != Some(&compiled) {
        return Err(format!(
            "compiled subjects differ (bless to update):\n{}",
            format_compiled(&compiled)
        ));
    }
    for check in &fixture.checks {
        let subject = compiled
            .get(&check.user)
            .ok_or_else(|| format!("check names unknown user {}", check.user))?;
        let now = check.now.unwrap_or(fixture.now);
        let got = subject.check(&check.node, &check.contexts, now);
        if got != check.expect {
            return Err(format!(
                "check {:?} in {{{}}} at {now}: expected {:?}, got {got:?}",
                check.node, check.contexts, check.expect
            ));
        }
        let traced = subject.trace(&check.node, &check.contexts, now).result;
        if traced != got {
            return Err(format!("trace of {:?} disagrees with check", check.node));
        }
    }
    for expectation in &fixture.meta {
        let subject = compiled
            .get(&expectation.user)
            .ok_or_else(|| format!("meta names unknown user {}", expectation.user))?;
        let got = subject.meta(
            &expectation.contexts,
            expectation.now.unwrap_or(fixture.now),
        );
        if got != expectation.expect {
            return Err(format!(
                "meta in {{{}}}: expected {:?}, got {got:?}",
                expectation.contexts, expectation.expect
            ));
        }
    }
    Ok(())
}

/// Add spaces after `:` and `,` outside strings, matching the corpus style.
fn spaced(compact: &str) -> String {
    let mut out = String::with_capacity(compact.len() + compact.len() / 4);
    let (mut in_string, mut escaped) = (false, false);
    for c in compact.chars() {
        out.push(c);
        if in_string {
            match (escaped, c) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => in_string = false,
                _ => {}
            }
        } else if c == '"' {
            in_string = true;
        } else if c == ':' || c == ',' {
            out.push(' ');
        }
    }
    out
}

fn format_list<T: Serialize>(name: &str, items: &[T]) -> String {
    if items.is_empty() {
        return format!("      \"{name}\": []");
    }
    let lines: Vec<String> = items
        .iter()
        .map(|item| format!("        {}", spaced(&serde_json::to_string(item).unwrap())))
        .collect();
    format!("      \"{name}\": [\n{}\n      ]", lines.join(",\n"))
}

fn format_compiled(compiled: &BTreeMap<Uuid, CompiledSubject>) -> String {
    let subjects: Vec<String> = compiled
        .iter()
        .map(|(uuid, s)| {
            let fields = [
                format!("      \"revision\": {}", s.revision),
                format!("      \"uuid\": \"{}\"", s.uuid),
                format!(
                    "      \"primary_group\": {}",
                    serde_json::to_string(&s.primary_group).unwrap()
                ),
                format_list("entries", &s.entries),
                format_list("prefix", &s.prefix),
                format_list("suffix", &s.suffix),
                format_list("meta", &s.meta),
            ];
            format!("    \"{uuid}\": {{\n{}\n    }}", fields.join(",\n"))
        })
        .collect();
    format!("{{\n{}\n  }}", subjects.join(",\n"))
}

/// The fixture text with its trailing `compiled` member replaced.
fn blessed(text: &str, compiled: &BTreeMap<Uuid, CompiledSubject>) -> String {
    let text = text.trim_end();
    let body = match text.find(",\n  \"compiled\":") {
        Some(at) => &text[..at],
        None => text
            .strip_suffix('}')
            .expect("fixture is an object")
            .trim_end(),
    };
    format!(
        "{body},\n  \"compiled\": {}\n}}\n",
        format_compiled(compiled)
    )
}

fn round_trip<T: DeserializeOwned + Serialize>(json: &Value) -> Result<Value, String> {
    let parsed: T = serde_json::from_value(json.clone()).map_err(|e| e.to_string())?;
    Ok(serde_json::to_value(parsed).unwrap())
}

fn changes(json: &Value) -> Result<Value, String> {
    let list: Vec<Change> = serde_json::from_value(json.clone()).map_err(|e| e.to_string())?;
    for change in &list {
        change.clone().normalized().map_err(|e| e.to_string())?;
    }
    Ok(serde_json::to_value(list).unwrap())
}

fn wire_fixture(fixture: &Value) -> Result<(), String> {
    let json = &fixture["json"];
    let parsed = match fixture["type"].as_str().unwrap_or_default() {
        "compiled_subject" => round_trip::<CompiledSubject>(json),
        "node" => round_trip::<Node>(json),
        "nodes" => round_trip::<Vec<Node>>(json),
        "context_set" => round_trip::<ContextSet>(json),
        "change" => changes(&Value::Array(vec![json.clone()])).map(|list| list[0].clone()),
        "changes" => changes(json),
        "change_event" => round_trip::<ChangeEvent>(json),
        "applied" => round_trip::<Applied>(json),
        "hello_request" => round_trip::<wire::HelloRequest>(json),
        "hello_reply" => round_trip::<wire::HelloReply>(json),
        "subject_get_request" => round_trip::<wire::SubjectGetRequest>(json),
        "subject_batch_request" => round_trip::<wire::SubjectBatchRequest>(json),
        "subject_batch_reply" => round_trip::<wire::SubjectBatchReply>(json),
        "groups_get_request" => round_trip::<wire::GroupsGetRequest>(json),
        "groups_reply" => round_trip::<wire::GroupsReply>(json),
        "apply_request" => round_trip::<wire::ApplyRequest>(json),
        "register_request" => round_trip::<wire::RegisterRequest>(json),
        "command_request" => round_trip::<wire::CommandRequest>(json),
        "command_reply" => round_trip::<wire::CommandReply>(json),
        "context_report" => round_trip::<wire::ContextReport>(json),
        "reply" => round_trip::<Reply<Value>>(json),
        "tristates" => round_trip::<Vec<Tristate>>(json),
        "meta_view" => round_trip::<MetaView>(json),
        "trace" => round_trip::<Trace>(json),
        other => return Err(format!("unknown wire type {other:?}")),
    };
    match (fixture["error"].as_bool().unwrap_or(false), parsed) {
        (true, Ok(value)) => Err(format!("expected a parse error, got {value}")),
        (true, Err(_)) => Ok(()),
        (false, Err(error)) => Err(error),
        (false, Ok(value)) => {
            let expect = fixture.get("expect").unwrap_or(json);
            if value == *expect {
                Ok(())
            } else {
                Err(format!("expected {expect}, got {value}"))
            }
        }
    }
}

fn luckperms_fixture(fixture: &Value) -> Result<(), String> {
    let node: luckperms::ExportNode =
        serde_json::from_value(fixture["node"].clone()).map_err(|e| e.to_string())?;
    let result = luckperms::parse_node(&node);
    match (fixture["error"].as_str(), result) {
        (Some("regex"), Err(Error::RegexNode { .. })) => Ok(()),
        (Some("invalid"), Err(error)) if !matches!(error, Error::RegexNode { .. }) => Ok(()),
        (None, Ok(nodes)) => {
            let expect: Vec<Node> =
                serde_json::from_value(fixture["expect"].clone()).map_err(|e| e.to_string())?;
            if nodes == expect {
                Ok(())
            } else {
                Err(format!("expected {expect:?}, got {nodes:?}"))
            }
        }
        (expect, got) => Err(format!("expected {expect:?}, got {got:?}")),
    }
}

#[test]
fn corpus() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/permissions");
    let bless = std::env::var_os("RIFT_BLESS_FIXTURES").is_some();
    let mut paths: Vec<_> = fs::read_dir(&dir)
        .expect("fixture directory")
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();
    let mut kinds = BTreeMap::<String, usize>::new();
    let mut failures = Vec::new();
    for path in &paths {
        let text = fs::read_to_string(path).unwrap();
        let fixture: Value =
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("{path:?}: {e}"));
        let kind = fixture["kind"].as_str().unwrap_or_default().to_owned();
        let stem = path.file_stem().unwrap().to_string_lossy();
        if fixture["name"]
            .as_str()
            .is_none_or(|name| !stem.ends_with(name))
        {
            failures.push(format!("{stem}: name must match the file name"));
        }
        let result = match kind.as_str() {
            "resolve" => resolve(path, &text, bless),
            "wire" => wire_fixture(&fixture),
            "luckperms" => luckperms_fixture(&fixture),
            other => Err(format!("unknown fixture kind {other:?}")),
        };
        if let Err(error) = result {
            failures.push(format!("{stem}: {error}"));
        }
        *kinds.entry(kind).or_default() += 1;
    }
    assert!(
        failures.is_empty(),
        "{} failing fixtures:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(kinds.get("resolve").is_some_and(|&n| n >= 25), "{kinds:?}");
    assert!(
        kinds.contains_key("wire") && kinds.contains_key("luckperms"),
        "{kinds:?}"
    );
}
