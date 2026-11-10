use super::*;
use crate::config::PermissionsConfig;

const NOW: u64 = 1_800_000_000;
const ALICE: Uuid = Uuid::from_u128(0xa11ce);
const BOB: Uuid = Uuid::from_u128(0xb0b);

fn args(line: &str) -> Vec<String> {
    split(line)
}

fn parsed(line: &str) -> Request {
    parse(&args(line), NOW).unwrap_or_else(|error| panic!("{line}: {}", error.message))
}

fn error(line: &str) -> String {
    parse(&args(line), NOW).unwrap_err().message
}

fn ctx(pairs: &[(&str, &str)]) -> ContextSet {
    ContextSet::from_pairs(pairs.iter().copied()).unwrap()
}

#[test]
fn splits_quotes_and_rejoins_backend_arguments() {
    assert_eq!(
        split(r#"user Alice meta setprefix 100 "&6[VIP] " server=survival"#),
        [
            "user",
            "Alice",
            "meta",
            "setprefix",
            "100",
            "&6[VIP] ",
            "server=survival"
        ]
    );
    assert_eq!(
        split(r#"a "" b "say \"hi\"""#),
        ["a", "", "b", "say \"hi\""]
    );
    assert_eq!(split("  spaced   out "), ["spaced", "out"]);
    let backend: Vec<String> = ["meta", "setprefix", "1", "\"&6[VIP]", "\""]
        .map(String::from)
        .to_vec();
    assert_eq!(rejoin(&backend), ["meta", "setprefix", "1", "&6[VIP] "]);
    let plain: Vec<String> = ["a", "b"].map(String::from).to_vec();
    assert_eq!(rejoin(&plain), plain);
}

#[test]
fn parses_times() {
    assert_eq!(parse_expiry("90", NOW), Ok(NOW + 90));
    assert_eq!(parse_expiry("30m", NOW), Ok(NOW + 1800));
    assert_eq!(parse_expiry("1d12h", NOW), Ok(NOW + 129_600));
    assert_eq!(parse_expiry("2W", NOW), Ok(NOW + 14 * 86_400));
    assert_eq!(parse_expiry("1mo", NOW), Ok(NOW + 30 * 86_400));
    assert_eq!(parse_expiry("1900000000", NOW), Ok(1_900_000_000));
    for bad in ["", "0", "0s", "5x", "m", "1700000000", "-5m", "1.5h"] {
        assert!(parse_expiry(bad, NOW).is_err(), "{bad}");
    }
}

#[test]
fn parses_user_commands() {
    assert_eq!(
        parsed("user Alice permission set essentials.fly"),
        Request::User {
            target: "Alice".into(),
            action: SubjectAction::Permission(PermissionAction::Set {
                node: "essentials.fly".into(),
                value: true,
                contexts: ContextSet::new(),
                expiry: None,
            }),
        }
    );
    assert_eq!(
        parsed("user Alice permission set fly false server=survival world=nether --duration 1h"),
        Request::User {
            target: "Alice".into(),
            action: SubjectAction::Permission(PermissionAction::Set {
                node: "fly".into(),
                value: false,
                contexts: ctx(&[("server", "survival"), ("world", "nether")]),
                expiry: Some(NOW + 3600),
            }),
        }
    );
    assert_eq!(
        parsed("user Alice permission settemp fly true 7d server=global"),
        Request::User {
            target: "Alice".into(),
            action: SubjectAction::Permission(PermissionAction::Set {
                node: "fly".into(),
                value: true,
                contexts: ContextSet::new(),
                expiry: Some(NOW + 7 * 86_400),
            }),
        }
    );
    assert_eq!(
        parsed("u Alice parent addtemp vip 30d region=spawn"),
        Request::User {
            target: "Alice".into(),
            action: SubjectAction::Parent(ParentAction::Add {
                group: "vip".into(),
                contexts: ctx(&[("region", "spawn")]),
                expiry: Some(NOW + 30 * 86_400),
            }),
        }
    );
    assert_eq!(
        parsed(r#"user Alice meta setprefix 100 "&6[VIP] ""#),
        Request::User {
            target: "Alice".into(),
            action: SubjectAction::Meta(MetaAction::SetDecoration {
                which: Decoration::Prefix,
                priority: 100,
                text: "&6[VIP] ".into(),
                contexts: ContextSet::new(),
                expiry: None,
            }),
        }
    );
    assert_eq!(
        parsed("user Alice promote ranks server=survival"),
        Request::User {
            target: "Alice".into(),
            action: SubjectAction::Promote {
                track: "ranks".into(),
                contexts: ctx(&[("server", "survival")]),
            },
        }
    );
    assert_eq!(
        parsed("user Alice clear"),
        Request::User {
            target: "Alice".into(),
            action: SubjectAction::Clear { contexts: None },
        }
    );
    assert_eq!(
        parsed("user Alice clear global"),
        Request::User {
            target: "Alice".into(),
            action: SubjectAction::Clear {
                contexts: Some(ContextSet::new()),
            },
        }
    );
    assert_eq!(
        parsed("user Alice"),
        Request::User {
            target: "Alice".into(),
            action: SubjectAction::Info,
        }
    );
}

#[test]
fn parses_group_track_and_other_commands() {
    assert_eq!(
        parsed("group vip create 10 Very Important"),
        Request::Group {
            name: "vip".into(),
            action: GroupAction::Create {
                weight: 10,
                display_name: Some("Very Important".into()),
            },
        }
    );
    assert_eq!(
        parsed("creategroup vip"),
        Request::Group {
            name: "vip".into(),
            action: GroupAction::Create {
                weight: 0,
                display_name: None,
            },
        }
    );
    assert_eq!(
        parsed("group default clear"),
        Request::Group {
            name: "default".into(),
            action: GroupAction::Subject(SubjectAction::Clear { contexts: None }),
        }
    );
    assert_eq!(
        parsed("group vip setdisplayname"),
        Request::Group {
            name: "vip".into(),
            action: GroupAction::SetDisplayName(None),
        }
    );
    assert_eq!(
        parsed("group vip permission unset kit.vip"),
        Request::Group {
            name: "vip".into(),
            action: GroupAction::Subject(SubjectAction::Permission(PermissionAction::Unset {
                node: "kit.vip".into(),
                contexts: ContextSet::new(),
                temporary: false,
            })),
        }
    );
    assert_eq!(
        parsed("track ranks insert mod 2"),
        Request::Track {
            name: "ranks".into(),
            action: TrackAction::Insert("mod".into(), 2),
        }
    );
    assert_eq!(
        parsed("createtrack ranks default vip"),
        Request::Track {
            name: "ranks".into(),
            action: TrackAction::Create(vec!["default".into(), "vip".into()]),
        }
    );
    assert_eq!(
        parsed("trace Alice fly server=lobby"),
        Request::Trace {
            target: "Alice".into(),
            node: "fly".into(),
            contexts: Some(ctx(&[("server", "lobby")])),
        }
    );
    assert_eq!(
        parsed("check Alice fly"),
        Request::Check {
            target: "Alice".into(),
            node: "fly".into(),
            contexts: None,
        }
    );
    assert_eq!(
        parsed("import lp.json.gz --replace"),
        Request::Import {
            file: "lp.json.gz".into(),
            replace: true,
        }
    );
    assert_eq!(
        parsed("log --since 4 --actor cli --subject group:vip --limit 5"),
        Request::Log {
            since: Some(4),
            actor: Some("cli".into()),
            subject: Some("group:vip".into()),
            limit: 5,
        }
    );
    let Request::Bulk(bulk) = parsed(
        "bulk users key essentials.% kind permission context server=a set-value false --dry-run",
    ) else {
        panic!("not bulk");
    };
    assert_eq!(bulk.filter.subjects, SubjectFilter::Users);
    assert_eq!(bulk.filter.key_like.as_deref(), Some("essentials.%"));
    assert_eq!(bulk.filter.kind, Some(NodeKind::Permission));
    assert_eq!(bulk.filter.contexts, Some(ctx(&[("server", "a")])));
    assert_eq!(bulk.patch.value, Some(false));
    assert!(bulk.dry_run);
    for (line, request) in [
        ("", Request::Help),
        ("listgroups", Request::ListGroups),
        ("listtracks", Request::ListTracks),
        ("sync", Request::Sync),
        ("backends", Request::Backends),
        ("registered", Request::Registered),
        ("doctor", Request::Doctor),
    ] {
        assert_eq!(parsed(line), request, "{line}");
    }
}

#[test]
fn reports_grammar_errors() {
    assert!(error("frobnicate").contains("unknown command"));
    assert!(error("user").contains("missing player"));
    assert!(error("user Alice permission").contains("missing permission action"));
    assert!(error("user Alice permission set").contains("missing permission node"));
    assert!(error("user Alice permission settemp fly 1h").contains("true or false"));
    assert!(error("user Alice permission settemp fly true").contains("missing time"));
    assert!(error("user Alice permission set fly true bogus").contains("contexts are key=value"));
    assert!(error("user Alice permission set fly true =x").contains("context"));
    assert!(error("user Alice parent addtemp vip soon").contains("invalid time"));
    assert!(error("user Alice meta setprefix high x").contains("whole number"));
    assert!(error("group vip promote ranks").contains("unknown action"));
    assert!(error("track ranks insert mod 0").contains("start at 1"));
    assert!(error("bulk users key a.%").contains("needs an action"));
    assert!(error("log --limit 0").contains("1 to 1000"));
    assert!(error("listgroups extra").contains("unexpected"));
    let failure = parse(&args("user Alice meta"), NOW).unwrap_err();
    assert!(
        failure
            .usage
            .iter()
            .any(|line| line.contains("meta setprefix"))
    );
}

#[test]
fn verbs_mirror_the_command_tree() {
    for (line, node) in [
        ("user A permission set x", "rift.perms.user.permission.set"),
        (
            "user A permission settemp x true 1h",
            "rift.perms.user.permission.settemp",
        ),
        ("user A parent add vip", "rift.perms.user.parent.add"),
        (
            "user A meta setsuffix 1 x",
            "rift.perms.user.meta.setsuffix",
        ),
        ("user A demote ranks", "rift.perms.user.demote"),
        (
            "group vip permission set x",
            "rift.perms.group.permission.set",
        ),
        ("group vip setweight 4", "rift.perms.group.setweight"),
        ("track ranks append vip", "rift.perms.track.append"),
        ("trace A x", "rift.perms.trace"),
        ("bulk delete", "rift.perms.bulk"),
    ] {
        assert_eq!(parsed(line).node(), node, "{line}");
    }
}

#[test]
fn covering_patterns_list_every_wildcard() {
    assert_eq!(covering_patterns("a.b.c"), ["a.b.c", "a.b.*", "a.*", "*"]);
    assert_eq!(covering_patterns("a.*"), ["a.*", "*"]);
    assert_eq!(covering_patterns("*"), ["*"]);
}

fn authority() -> Arc<Authority> {
    let authority = Arc::new(Authority::memory(&PermissionsConfig::default()));
    authority.join(ALICE, "Alice", "survival");
    authority.join(BOB, "Bob", "survival");
    authority
}

fn console(env: &Env, line: &str) -> Output {
    let output = run(env, &Sender::Console(Actor::Cli), parsed(line));
    assert!(output.ok, "{line}: {}", output.text());
    output
}

fn as_player(env: &Env, uuid: Uuid, line: &str) -> Output {
    run(env, &Sender::Player(uuid), parsed(line))
}

#[test]
fn console_edits_users_groups_and_tracks() {
    let env = Env::new(authority());
    let authority = &env.authority;
    console(&env, "group vip create 10");
    console(&env, "group mod create 20");
    console(&env, "group vip permission set kit.vip");
    console(&env, r#"group vip meta setprefix 10 "&6[VIP] ""#);
    let output = console(&env, "user Alice parent add vip");
    assert!(
        output.text().contains("now inherits vip"),
        "{}",
        output.text()
    );
    assert_eq!(output.revision, authority.revision());
    assert_eq!(
        authority.check(ALICE, "kit.vip", &ContextSet::new()),
        Tristate::True
    );
    let again = console(&env, "user Alice parent add vip");
    assert!(
        again.text().contains("already inherits"),
        "{}",
        again.text()
    );
    console(
        &env,
        "user Alice permission set essentials.fly true server=survival",
    );
    let check = console(&env, "check Alice essentials.fly");
    assert!(check.text().contains("= true"), "{}", check.text());
    let elsewhere = console(&env, "check Alice essentials.fly server=lobby");
    assert!(
        elsewhere.text().contains("undefined"),
        "{}",
        elsewhere.text()
    );

    let trace = console(&env, "trace Alice kit.vip");
    let text = trace.text();
    assert!(
        text.contains("Winner: kit.vip = true from group vip"),
        "{text}"
    );

    console(&env, "createtrack ranks default vip");
    console(&env, "track ranks append mod");
    let promoted = console(&env, "user Alice promote ranks");
    assert!(promoted.text().contains("vip → mod"), "{}", promoted.text());
    let info = console(&env, "user Alice info");
    assert!(info.text().contains("parent mod"), "{}", info.text());
    assert!(
        info.text().contains("Online on survival"),
        "{}",
        info.text()
    );

    console(&env, "user Bob parent set vip");
    let members = console(&env, "group vip listmembers");
    assert!(members.text().contains("Bob"), "{}", members.text());

    let preview = console(&env, "bulk all key kit.% set-value false --dry-run");
    assert!(
        preview.text().contains("1 nodes would change"),
        "{}",
        preview.text()
    );
    assert_eq!(
        authority.check(ALICE, "kit.vip", &ContextSet::new()),
        Tristate::Undefined,
        "Alice moved to mod, which does not inherit vip"
    );
    assert_eq!(
        authority.check(BOB, "kit.vip", &ContextSet::new()),
        Tristate::True
    );
    console(&env, "bulk all key kit.% set-value false");
    assert_eq!(
        authority.check(BOB, "kit.vip", &ContextSet::new()),
        Tristate::False
    );

    let log = console(&env, "log --limit 3");
    assert!(log.text().contains("by cli"), "{}", log.text());
    let groups = console(&env, "listgroups");
    assert!(groups.text().starts_with("3 groups"), "{}", groups.text());
    let search = console(&env, "search kit.vip");
    assert!(search.text().contains("group vip"), "{}", search.text());
    let compiled = console(&env, "compiled Bob");
    assert_eq!(compiled.data.unwrap()["uuid"], BOB.to_string());
}

#[test]
fn failures_explain_themselves() {
    let env = Env::new(authority());
    let unknown = run(
        &env,
        &Sender::Console(Actor::Cli),
        parsed("user Nobody info"),
    );
    assert!(!unknown.ok);
    assert!(
        unknown.text().contains("Unknown player"),
        "{}",
        unknown.text()
    );
    let missing = run(
        &env,
        &Sender::Console(Actor::Cli),
        parsed("user Alice parent add ghosts"),
    );
    assert!(!missing.ok);
    assert!(
        missing.text().contains("unknown group"),
        "{}",
        missing.text()
    );
    console(&env, "user Alice permission set fly");
    let permanent = run(
        &env,
        &Sender::Console(Actor::Cli),
        parsed("user Alice permission unsettemp fly"),
    );
    assert!(!permanent.ok);
    assert!(
        permanent.text().contains("permanent"),
        "{}",
        permanent.text()
    );
    let unset = console(&env, "user Alice permission unset nothing.here");
    assert!(unset.text().contains("does not have"), "{}", unset.text());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let output = rt.block_on(execute(
        &env,
        &Sender::Console(Actor::Cli),
        &args("user Alice permission settemp fly maybe"),
    ));
    assert!(!output.ok);
    assert!(output.text().contains("settemp needs true or false"));
    assert!(output.lines.iter().any(|line| line.tone == Tone::Muted));
    assert_eq!(output.component()["extra"][0]["color"], "red");
    let mut theme = crate::text::Styles::default();
    theme.set("bad", "<#ff0000><bold>").unwrap();
    let styled = output.component_with(&theme);
    assert_eq!(styled["extra"][0]["color"], "#ff0000");
    assert_eq!(styled["extra"][0]["bold"], true);
}

#[test]
fn players_need_the_verb_node_and_cannot_escalate() {
    let env = Env::new(authority());
    let authority = &env.authority;
    console(&env, "group admin create 100");
    console(&env, "group admin permission set server.admin");
    let denied = as_player(&env, ALICE, "user Bob permission set chat.color");
    assert!(!denied.ok);
    assert!(
        denied.text().contains("rift.perms.user.permission.set"),
        "{}",
        denied.text()
    );
    console(&env, "user Alice permission set rift.perms.user.*");
    console(&env, "user Alice permission set chat.color");
    let granted = as_player(&env, ALICE, "user Bob permission set chat.color");
    assert!(granted.ok, "{}", granted.text());
    assert_eq!(
        authority.check(BOB, "chat.color", &ContextSet::new()),
        Tristate::True
    );
    // Revoking and denying are not escalation.
    let deny = as_player(&env, ALICE, "user Bob permission set server.admin false");
    assert!(deny.ok, "{}", deny.text());
    let escalate = as_player(&env, ALICE, "user Bob permission set server.admin true");
    assert!(!escalate.ok);
    assert!(
        escalate.text().contains("rift.perms.escalate"),
        "{}",
        escalate.text()
    );
    let group = as_player(&env, ALICE, "user Bob parent add admin");
    assert!(!group.ok);
    assert!(group.text().contains("the group admin"), "{}", group.text());
    let wildcard = as_player(&env, ALICE, "user Bob permission set chat.*");
    assert!(!wildcard.ok, "chat.color does not cover chat.*");
    console(&env, "user Alice permission set rift.perms.escalate");
    let allowed = as_player(&env, ALICE, "user Bob parent add admin");
    assert!(allowed.ok, "{}", allowed.text());
    let other_verb = as_player(&env, ALICE, "group admin delete");
    assert!(!other_verb.ok);
    assert!(other_verb.text().contains("rift.perms.group.delete"));
    let help = as_player(&env, BOB, "help");
    assert!(help.ok);
    let file = as_player(&env, ALICE, "user Alice info");
    assert!(file.ok);
    console(&env, "user Alice permission set rift.perms.export");
    let path = as_player(&env, ALICE, "export ../escape.json");
    assert!(!path.ok);
    assert!(path.text().contains("without a path"), "{}", path.text());
}

#[test]
fn export_and_import_round_trip_through_files() {
    let env = Env::new(authority());
    console(&env, "group vip create 5");
    console(&env, "group vip permission set kit.vip");
    console(&env, "user Alice parent add vip");
    let dir = std::env::temp_dir().join(format!("rift-perms-command-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("export.json.gz");
    let _ = std::fs::remove_file(&file);
    let line = format!("export {}", file.display());
    let exported = console(&env, &line);
    assert!(
        exported.text().contains("Exported 2 groups"),
        "{}",
        exported.text()
    );
    let again = run(&env, &Sender::Console(Actor::Cli), parsed(&line));
    assert!(!again.ok, "export never overwrites");

    let fresh = Env::new(Arc::new(Authority::memory(&PermissionsConfig::default())));
    let imported = console(&fresh, &format!("import {} --replace", file.display()));
    assert!(
        imported.text().contains("Imported 2 groups"),
        "{}",
        imported.text()
    );
    assert_eq!(
        fresh.authority.check(ALICE, "kit.vip", &ContextSet::new()),
        Tristate::True
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn doctor_finds_dangling_unreachable_and_ungranted() {
    let mut env = Env::new(authority());
    console(&env, "group orphan create");
    console(&env, "group vip create");
    console(&env, "user Alice parent add vip");
    console(&env, "group vip delete");
    env.commands = vec![
        ("fly".into(), "essentials.fly".into()),
        ("spawn".into(), "essentials.spawn".into()),
    ];
    console(&env, "group default permission set essentials.*");
    let output = console(&env, "doctor");
    let text = output.text();
    assert!(text.contains("missing group vip"), "{text}");
    assert!(text.contains("nobody inherits group orphan"), "{text}");
    assert!(!text.contains("/fly"), "covered by essentials.*: {text}");
    console(&env, "group default permission unset essentials.*");
    let text = console(&env, "doctor").text();
    assert!(text.contains("/spawn needs essentials.spawn"), "{text}");
}

#[test]
fn ungranted_command_permissions_reads_config_groups() {
    let mut group = rift_permissions::Group::new("staff").unwrap();
    group
        .nodes
        .push(Node::permission("network.*", true).unwrap());
    let groups = BTreeMap::from([("staff".to_owned(), group)]);
    let commands = vec![
        ("ban".to_owned(), "network.ban".to_owned()),
        ("fly".to_owned(), "essentials.fly".to_owned()),
    ];
    let missing = super::super::ungranted_command_permissions(None, &groups, &commands).unwrap();
    assert_eq!(missing, [("fly".to_owned(), "essentials.fly".to_owned())]);
}
