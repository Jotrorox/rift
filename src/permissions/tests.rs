use super::*;
use crate::{
    config::{Config, PermissionsConfig},
    database::Database,
    extensions::{Action, Context, Extensions},
    messaging::Broker,
};
use rift_permissions::{
    Actor, Change, ContextSet, Node, NodeKind, SubjectRef, Tristate, Uuid,
    change::{Filter, Op, Patch},
    node::NodeKey,
    unix_now,
    wire::{PermissionDefault, RegisteredPermission},
};
use std::{path::PathBuf, sync::Arc, time::Duration};

const ALICE: Uuid = Uuid::from_u128(0x0101_0101_0101_0101_0101_0101_0101_0101);
const BOB: Uuid = Uuid::from_u128(0x0202_0202_0202_0202_0202_0202_0202_0202);

fn authority() -> Authority {
    Authority::memory(&PermissionsConfig::default())
}

fn user(uuid: Uuid) -> SubjectRef {
    SubjectRef::User(uuid)
}

fn group(name: &str) -> SubjectRef {
    SubjectRef::Group(name.into())
}

fn set(subject: SubjectRef, node: Node) -> Change {
    Change::SetNode { subject, node }
}

fn perm(key: &str, value: bool) -> Node {
    Node::permission(key, value).unwrap()
}

fn parent(name: &str) -> Node {
    Node::inherit(name, true).unwrap()
}

fn create(name: &str, weight: i32) -> Change {
    Change::CreateGroup {
        name: name.into(),
        weight,
        display_name: None,
    }
}

fn global() -> ContextSet {
    ContextSet::new()
}

fn survival() -> ContextSet {
    ContextSet::of("server", "survival").unwrap()
}

/// A directory removed when the test ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("rift-permissions-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn open(&self) -> Authority {
        let database = Arc::new(Database::open(&self.0.join("rift.sqlite3")).unwrap());
        Authority::open(database, &PermissionsConfig::default()).unwrap()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn store_round_trips_and_revisions_survive_reopening() {
    let scratch = Scratch::new("reopen");
    let authority = scratch.open();
    assert_eq!(authority.revision(), 0);
    assert!(
        authority.group("default").is_some(),
        "default exists from the start"
    );
    let expiry = unix_now() + 3600;
    let nodes = [
        perm("essentials.fly", true)
            .with_contexts(survival())
            .unwrap()
            .expiring_at(expiry),
        parent("vip"),
        Node::prefix(5, "&a").unwrap(),
        Node::meta("homes", "3").unwrap(),
    ];
    let mut changes = vec![
        Change::CreateGroup {
            name: "VIP".into(),
            weight: 10,
            display_name: Some("V.I.P.".into()),
        },
        Change::SetTrack {
            name: "ranks".into(),
            groups: vec!["default".into(), "vip".into()],
        },
        Change::SetPrimaryGroup {
            user: ALICE,
            group: Some("vip".into()),
        },
    ];
    changes.extend(nodes.iter().cloned().map(|node| set(user(ALICE), node)));
    for (index, change) in changes.into_iter().enumerate() {
        let applied = authority.apply(Actor::Cli, change).unwrap();
        assert_eq!(applied.revision, index as u64 + 1);
        assert!(applied.ops > 0);
    }
    drop(authority);
    let reopened = scratch.open();
    assert_eq!(reopened.revision(), 7);
    let vip = reopened.group("vip").unwrap();
    assert_eq!(
        (vip.weight, vip.display_name.as_deref()),
        (10, Some("V.I.P."))
    );
    assert_eq!(
        vip.nodes.iter().map(Node::kind).collect::<Vec<_>>(),
        [NodeKind::Weight, NodeKind::DisplayName]
    );
    let stored = reopened.user(ALICE).unwrap();
    assert_eq!(stored.nodes, nodes);
    assert_eq!(stored.primary_group.as_deref(), Some("vip"));
    assert_eq!(reopened.tracks()["ranks"].groups, ["default", "vip"]);
    assert_eq!(
        reopened.check(ALICE, "essentials.fly", &survival()),
        Tristate::True
    );
    assert_eq!(
        reopened.check(ALICE, "essentials.fly", &global()),
        Tristate::Undefined
    );
    let applied = reopened
        .apply(Actor::Cli, set(user(BOB), perm("chat", true)))
        .unwrap();
    assert_eq!(applied.revision, 8, "revisions continue after a restart");
    let log = reopened.log(&LogQuery::default()).unwrap();
    assert_eq!(
        log.iter().map(|e| e.revision).collect::<Vec<_>>(),
        (1..=8).collect::<Vec<_>>()
    );
    assert!(log.iter().all(|e| e.actor == Actor::Cli));
}

#[test]
fn writes_that_change_nothing_record_nothing() {
    let authority = authority();
    let node = perm("chat.color", true);
    let first = authority
        .apply(Actor::Cli, set(user(ALICE), node.clone()))
        .unwrap();
    assert_eq!((first.revision, first.ops), (1, 1));
    assert_eq!(first.changed_users, [ALICE]);
    for change in [
        set(user(ALICE), node.clone()),
        Change::UnsetNode {
            subject: user(BOB),
            key: node.key(),
        },
        Change::UnsetNode {
            subject: user(ALICE),
            key: NodeKey::new(NodeKind::Permission, "other", global()).unwrap(),
        },
        Change::SetWeight {
            group: "default".into(),
            weight: 0,
        },
        Change::ClearSubject {
            subject: user(BOB),
            contexts: None,
        },
        Change::Bulk {
            filter: Filter {
                key_like: Some("nothing.%".into()),
                ..Filter::default()
            },
            patch: Patch {
                delete: true,
                ..Patch::default()
            },
        },
    ] {
        let applied = authority.apply(Actor::Cli, change.clone()).unwrap();
        assert_eq!((applied.revision, applied.ops), (1, 0), "{change:?}");
    }
    assert_eq!(authority.log(&LogQuery::default()).unwrap().len(), 1);
    // Replacing a node's value is one op with both sides.
    let applied = authority
        .apply(Actor::Cli, set(user(ALICE), perm("chat.color", false)))
        .unwrap();
    assert_eq!(applied.revision, 2);
    let log = authority.log(&LogQuery::default()).unwrap();
    assert_eq!(
        log[1].ops,
        [Op::Node {
            subject: user(ALICE),
            before: Some(node),
            after: Some(perm("chat.color", false)),
        }]
    );
}

#[test]
fn invalid_changes_and_limits_fail_without_writing() {
    let authority = authority();
    authority.configure(&PermissionsConfig {
        limits: rift_permissions::Limits {
            groups: 3,
            user_nodes: 2,
            parents: 1,
            contexts: 1,
            tracks: 1,
            ..Default::default()
        },
        ..PermissionsConfig::default()
    });
    authority.apply(Actor::Cli, create("vip", 0)).unwrap();
    let base = authority.revision();
    let failures: Vec<(Change, rift_permissions::wire::ErrorCode)> = vec![
        (
            set(group("missing"), perm("a", true)),
            rift_permissions::wire::ErrorCode::NotFound,
        ),
        (
            set(user(ALICE), Node::weight(3)),
            rift_permissions::wire::ErrorCode::InvalidRequest,
        ),
        (
            Change::DeleteGroup {
                name: "default".into(),
            },
            rift_permissions::wire::ErrorCode::InvalidRequest,
        ),
        (
            create("vip", 1),
            rift_permissions::wire::ErrorCode::Conflict,
        ),
        (
            set(user(ALICE), perm("a", true).expiring_at(1)),
            rift_permissions::wire::ErrorCode::Conflict,
        ),
        (
            set(
                user(ALICE),
                perm("a", true)
                    .with_contexts(ContextSet::parse("region=a;server=b").unwrap())
                    .unwrap(),
            ),
            rift_permissions::wire::ErrorCode::Limit,
        ),
        (
            Change::SetPrimaryGroup {
                user: ALICE,
                group: Some("ghost".into()),
            },
            rift_permissions::wire::ErrorCode::NotFound,
        ),
        (
            Change::Promote {
                user: ALICE,
                track: "missing".into(),
                contexts: global(),
            },
            rift_permissions::wire::ErrorCode::NotFound,
        ),
        (
            Change::Batch {
                changes: vec![
                    set(user(ALICE), perm("first", true)),
                    set(group("missing"), perm("a", true)),
                ],
            },
            rift_permissions::wire::ErrorCode::NotFound,
        ),
    ];
    for (change, code) in failures {
        let error = authority.apply(Actor::Cli, change.clone()).unwrap_err();
        assert_eq!(error.code(), code, "{change:?}: {error}");
    }
    assert_eq!(authority.revision(), base, "failed changes write nothing");
    assert!(authority.user(ALICE).unwrap().nodes.is_empty());
    // Limits count what is stored.
    authority
        .apply(Actor::Cli, set(user(ALICE), parent("vip")))
        .unwrap();
    let parents = authority
        .apply(Actor::Cli, set(user(ALICE), parent("default")))
        .unwrap_err();
    assert!(
        matches!(parents, PermError::Limit { limit: 1, .. }),
        "{parents}"
    );
    authority
        .apply(Actor::Cli, set(user(ALICE), perm("a", true)))
        .unwrap();
    let nodes = authority
        .apply(Actor::Cli, set(user(ALICE), perm("b", true)))
        .unwrap_err();
    assert!(
        matches!(nodes, PermError::Limit { limit: 2, .. }),
        "{nodes}"
    );
    // Replacing a node in its slot does not count against the limit.
    authority
        .apply(Actor::Cli, set(user(ALICE), perm("a", false)))
        .unwrap();
    authority.apply(Actor::Cli, create("mod", 0)).unwrap();
    assert!(matches!(
        authority.apply(Actor::Cli, create("admin", 0)).unwrap_err(),
        PermError::Limit { what: "groups", .. }
    ));
    let track = |name: &str| Change::SetTrack {
        name: name.into(),
        groups: vec!["vip".into()],
    };
    authority.apply(Actor::Cli, track("one")).unwrap();
    assert!(matches!(
        authority.apply(Actor::Cli, track("two")).unwrap_err(),
        PermError::Limit { what: "tracks", .. }
    ));
}

#[test]
fn group_changes_invalidate_dependent_users_transitively() {
    let authority = authority();
    for change in [
        create("mod", 0),
        create("staff", 0),
        set(group("staff"), parent("mod")),
        set(user(ALICE), parent("staff")),
        set(user(ALICE), parent("ghost")),
        set(user(BOB), perm("own", true)),
    ] {
        authority.apply(Actor::Cli, change).unwrap();
    }
    assert_eq!(
        authority.check(ALICE, "kick", &global()),
        Tristate::Undefined
    );
    let bob = authority.compiled(BOB);
    assert!(Arc::ptr_eq(&bob, &authority.compiled(BOB)), "cached");
    // A grandparent's change reaches Alice; Bob's subject is untouched.
    authority
        .apply(Actor::Cli, set(group("mod"), perm("kick", true)))
        .unwrap();
    assert_eq!(authority.check(ALICE, "kick", &global()), Tristate::True);
    assert!(Arc::ptr_eq(&bob, &authority.compiled(BOB)));
    // A dangling parent resolves once the group exists.
    authority.apply(Actor::Cli, create("ghost", 100)).unwrap();
    authority
        .apply(Actor::Cli, set(group("ghost"), perm("kick", false)))
        .unwrap();
    assert_eq!(authority.check(ALICE, "kick", &global()), Tristate::False);
    // Weights reorder: mod outranks ghost now.
    authority
        .apply(
            Actor::Cli,
            Change::SetWeight {
                group: "mod".into(),
                weight: 200,
            },
        )
        .unwrap();
    assert_eq!(authority.check(ALICE, "kick", &global()), Tristate::True);
    // Changing default affects everyone.
    authority
        .apply(Actor::Cli, set(group("default"), perm("chat", true)))
        .unwrap();
    assert_eq!(authority.check(BOB, "chat", &global()), Tristate::True);
    assert!(!Arc::ptr_eq(&bob, &authority.compiled(BOB)));
    // Deleting a group leaves its inherit nodes dangling.
    authority
        .apply(Actor::Cli, Change::DeleteGroup { name: "mod".into() })
        .unwrap();
    assert_eq!(authority.check(ALICE, "kick", &global()), Tristate::False);
    assert!(authority.group("mod").is_none());
    assert_eq!(authority.compiled(ALICE).revision, authority.revision());
}

#[test]
fn committed_changes_notify_with_fresh_subjects_for_online_players() {
    let authority = Arc::new(authority());
    let mut notifications = authority.subscribe();
    authority.join(ALICE, "Alice", "lobby", None);
    let Ok(Notification::Joined(joined)) = notifications.try_recv() else {
        panic!("a join pushes the subject")
    };
    assert_eq!(joined.uuid, ALICE);
    authority.apply(Actor::Cli, create("vip", 0)).unwrap();
    authority
        .apply(Actor::Cli, set(user(ALICE), parent("vip")))
        .unwrap();
    authority
        .apply(Actor::Cli, set(user(BOB), parent("vip")))
        .unwrap();
    authority.compiled(BOB);
    while notifications.try_recv().is_ok() {}
    let applied = authority
        .apply(
            Actor::Operator("alice".into()),
            set(group("vip"), perm("fly", true)),
        )
        .unwrap();
    // Bob is cached but offline: listed as changed only when online.
    assert_eq!(applied.changed_users, [ALICE]);
    assert_eq!(applied.changed_groups, ["vip"]);
    let Ok(Notification::Committed { event, subjects }) = notifications.try_recv() else {
        panic!("a commit notifies")
    };
    assert_eq!(event.revision, applied.revision);
    assert_eq!(event.actor, Actor::Operator("alice".into()));
    assert_eq!(
        (event.changed.clone(), event.groups.clone()),
        (vec![ALICE], vec!["vip".into()])
    );
    assert_eq!(subjects.len(), 1);
    assert_eq!(subjects[0].revision, applied.revision);
    assert_eq!(
        subjects[0].check("fly", &global(), unix_now()),
        Tristate::True
    );
    // Contexts: the proxy's own, reported ones (allowlisted) and provided ones.
    authority.configure(&PermissionsConfig {
        external_contexts: ["region".to_owned()].into(),
        ..PermissionsConfig::default()
    });
    let region = ContextSet::of("region", "spawn").unwrap();
    assert!(
        authority
            .report_contexts(ALICE, "lobby", region.clone())
            .unwrap()
    );
    assert!(
        !authority
            .report_contexts(ALICE, "survival", region.clone())
            .unwrap()
    );
    assert!(matches!(
        authority
            .report_contexts(ALICE, "lobby", ContextSet::of("rank", "x").unwrap())
            .unwrap_err(),
        PermError::Forbidden(_)
    ));
    authority
        .set_proxy_context(ALICE, "world", Some("nether"))
        .unwrap();
    assert!(
        authority
            .set_proxy_context(ALICE, "region", Some("x"))
            .is_err()
    );
    authority.provide_contexts(ALICE, ContextSet::of("plot", "7").unwrap());
    assert_eq!(
        authority.contexts(ALICE).to_string(),
        "plot=7;region=spawn;server=lobby;world=nether"
    );
    authority
        .apply(
            Actor::Cli,
            set(
                user(ALICE),
                perm("build", true).with_contexts(region).unwrap(),
            ),
        )
        .unwrap();
    assert_eq!(authority.check_active(ALICE, "build"), Tristate::True);
    authority.switch_server(ALICE, "survival", None);
    assert_eq!(authority.contexts(ALICE).to_string(), "server=survival");
    assert_eq!(authority.check_active(ALICE, "build"), Tristate::Undefined);
    assert_eq!(authority.user_by_name("ALICE").unwrap(), Some(ALICE));
    authority.leave(ALICE);
    assert!(authority.online().is_empty());
    assert!(authority.contexts(ALICE).is_empty());
    // The last-seen name outlives the session.
    assert_eq!(authority.user_by_name("alice").unwrap(), Some(ALICE));
    assert_eq!(authority.user_by_name("nobody").unwrap(), None);
}

#[test]
fn promote_and_demote_move_along_tracks_in_their_contexts() {
    let authority = authority();
    for name in ["vip", "mod"] {
        authority.apply(Actor::Cli, create(name, 0)).unwrap();
    }
    authority
        .apply(
            Actor::Cli,
            Change::SetTrack {
                name: "ranks".into(),
                groups: vec!["default".into(), "vip".into(), "mod".into()],
            },
        )
        .unwrap();
    let parents = |authority: &Authority| -> Vec<(String, String)> {
        authority
            .user(ALICE)
            .unwrap()
            .nodes
            .into_iter()
            .map(|n| (n.payload.key(), n.contexts.to_string()))
            .collect()
    };
    let promote = |contexts: &ContextSet| authority.promote(Actor::Cli, ALICE, "ranks", contexts);
    promote(&global()).unwrap();
    assert_eq!(parents(&authority), [("vip".into(), String::new())]);
    promote(&global()).unwrap();
    assert_eq!(parents(&authority), [("mod".into(), String::new())]);
    let top = promote(&global()).unwrap_err();
    assert_eq!(top.code(), rift_permissions::wire::ErrorCode::Conflict);
    // Contexts are separate positions on the same track.
    promote(&survival()).unwrap();
    assert_eq!(
        parents(&authority),
        [
            ("mod".into(), String::new()),
            ("vip".into(), "server=survival".into())
        ]
    );
    let demote = |contexts: &ContextSet| authority.demote(Actor::Cli, ALICE, "ranks", contexts);
    demote(&global()).unwrap();
    demote(&global()).unwrap();
    assert_eq!(
        parents(&authority),
        [("vip".into(), "server=survival".into())]
    );
    assert!(demote(&global()).is_err(), "already at the bottom");
    assert!(
        authority
            .check(ALICE, "group.vip", &survival())
            .as_bool(false)
    );
    let log = authority.log(&LogQuery::default()).unwrap();
    // A promotion from vip to mod is one revision with both ops.
    assert!(log.iter().any(|event| event.ops.len() == 2));
}

#[test]
fn bulk_edits_preview_then_apply_in_one_revision() {
    let authority = authority();
    authority.apply(Actor::Cli, create("vip", 0)).unwrap();
    for change in [
        set(user(ALICE), perm("essentials.fly", true)),
        set(user(BOB), perm("essentials.heal", true)),
        set(group("vip"), perm("essentials.home", true)),
        set(group("vip"), perm("worldedit.wand", true)),
    ] {
        authority.apply(Actor::Cli, change).unwrap();
    }
    let filter = Filter {
        kind: Some(NodeKind::Permission),
        key_like: Some("Essentials.%".into()),
        ..Filter::default()
    };
    let deny = Patch {
        value: Some(false),
        ..Patch::default()
    };
    let before = authority.revision();
    let Bulk::Preview(ops) = authority
        .bulk(Actor::Cli, filter.clone(), deny.clone(), true)
        .unwrap()
    else {
        panic!("a dry run previews")
    };
    assert_eq!(ops.len(), 3);
    assert_eq!(authority.revision(), before, "a dry run writes nothing");
    assert_eq!(
        authority.check(ALICE, "essentials.fly", &global()),
        Tristate::True
    );
    let Bulk::Applied(applied) = authority
        .bulk(Actor::Cli, filter.clone(), deny, false)
        .unwrap()
    else {
        panic!("applied")
    };
    assert_eq!((applied.revision, applied.ops), (before + 1, 3));
    assert_eq!(
        authority.check(ALICE, "essentials.fly", &global()),
        Tristate::False
    );
    assert_eq!(
        authority.check(BOB, "essentials.heal", &global()),
        Tristate::False
    );
    assert_eq!(
        authority.check(BOB, "worldedit.wand", &global()),
        Tristate::Undefined
    );
    // Moving nodes to another context, then deleting by context.
    let moved = Patch {
        contexts: Some(survival()),
        ..Patch::default()
    };
    authority
        .bulk(Actor::Cli, filter.clone(), moved, false)
        .unwrap();
    let holders = authority.search(&filter, 10).unwrap();
    assert_eq!(holders.len(), 3);
    assert!(holders.iter().all(|h| h.node.contexts == survival()));
    let remove = Filter {
        contexts: Some(survival()),
        subjects: rift_permissions::change::SubjectFilter::Users,
        ..Filter::default()
    };
    let delete = Patch {
        delete: true,
        ..Patch::default()
    };
    authority.bulk(Actor::Cli, remove, delete, false).unwrap();
    assert_eq!(authority.search(&filter, 10).unwrap().len(), 1);
    assert_eq!(
        authority.search(&filter, 10).unwrap()[0].subject,
        group("vip")
    );
}

fn luckperms_export() -> Vec<u8> {
    std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/luckperms-export.json"
    ))
    .unwrap()
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

#[test]
fn luckperms_exports_import_in_one_revision_and_round_trip() {
    let notch = Uuid::parse("069a79f4-44e9-4726-a5be-fca90e38aaf5").unwrap();
    let jeb = Uuid::parse("853c80ef-3c37-49fd-aa49-938b674adae6").unwrap();
    let authority = authority();
    let report = authority
        .import_luckperms(Actor::Import, &gzip(&luckperms_export())[..], false)
        .unwrap();
    assert_eq!(report.revision, 1);
    assert_eq!((report.groups, report.users, report.tracks), (4, 2, 1));
    let skipped: Vec<(&str, &str)> = report
        .skipped
        .iter()
        .map(|s| (s.subject.as_str(), s.key.as_str()))
        .collect();
    assert_eq!(
        skipped,
        [
            ("group:mod", "R=essentials\\.warp\\..*"),
            ("user:not-a-uuid", ""),
            ("user:853c80ef-3c37-49fd-aa49-938b674adae6", "old.reward"),
        ]
    );
    let check = |uuid, node: &str, contexts: &ContextSet| authority.check(uuid, node, contexts);
    // Specificity beats rank: mod's exact deny wins over admin's `*`.
    assert_eq!(check(notch, "anything.at.all", &global()), Tristate::True);
    assert_eq!(check(notch, "essentials.ban", &global()), Tristate::False);
    assert_eq!(check(jeb, "essentials.fly", &survival()), Tristate::True);
    assert_eq!(check(jeb, "essentials.fly", &global()), Tristate::Undefined);
    let creative = ContextSet::of("server", "creative").unwrap();
    assert_eq!(check(jeb, "kit.vip", &creative), Tristate::True);
    assert_eq!(check(jeb, "essentials.sethome", &global()), Tristate::True);
    let world = ContextSet::of("world", "creative_world").unwrap();
    assert_eq!(check(jeb, "worldedit.wand", &world), Tristate::True);
    let meta = authority.meta(notch, &global());
    assert_eq!(meta.prefix.as_deref(), Some("&c[Admin] "));
    assert_eq!(meta.meta["homes"], "5");
    let meta = authority.meta(jeb, &global());
    assert_eq!(
        (meta.primary_group.as_str(), meta.suffix.as_deref()),
        ("vip", Some(" ✦"))
    );
    assert_eq!(
        authority.group("vip").unwrap().display_name.as_deref(),
        Some("VIP")
    );
    assert_eq!(authority.group("admin").unwrap().weight, 100);
    assert_eq!(authority.user_by_name("JEB_").unwrap(), Some(jeb));
    assert_eq!(authority.tracks()["staff"].groups.len(), 4);
    assert!(
        authority
            .user(jeb)
            .unwrap()
            .nodes
            .iter()
            .all(|n| n.source == rift_permissions::Source::Import)
    );
    // Importing the same file again changes nothing.
    let again = authority
        .import_luckperms(Actor::Import, &luckperms_export()[..], false)
        .unwrap();
    assert_eq!((again.revision, again.ops), (1, 0));
    // Replace drops earlier imported nodes, never runtime ones.
    authority
        .apply(Actor::Cli, set(user(jeb), perm("runtime.node", true)))
        .unwrap();
    let mut smaller: serde_json::Value = serde_json::from_slice(&luckperms_export()).unwrap();
    smaller["users"]["853c80ef-3c37-49fd-aa49-938b674adae6"]["nodes"] = serde_json::json!([]);
    let replaced = authority
        .import_luckperms(Actor::Import, smaller.to_string().as_bytes(), true)
        .unwrap();
    assert_eq!(replaced.revision, 3, "one revision for the whole import");
    assert_eq!(
        authority.user(jeb).unwrap().nodes,
        [perm("runtime.node", true)]
    );
    assert_eq!(check(notch, "essentials.ban", &global()), Tristate::False);
    // Export, then import into an empty store: the same answers.
    let export = authority.export_luckperms().unwrap();
    let mut file = Vec::new();
    write_export(&export, &mut file, true).unwrap();
    let copy = self::authority();
    copy.import_luckperms(Actor::Import, &file[..], false)
        .unwrap();
    for (uuid, node) in [
        (notch, "essentials.ban"),
        (notch, "x"),
        (jeb, "runtime.node"),
    ] {
        assert_eq!(
            copy.check(uuid, node, &global()),
            check(uuid, node, &global())
        );
    }
    assert_eq!(copy.group("vip").unwrap().weight, 10);
    assert!(matches!(
        authority.import_luckperms(Actor::Import, &b"{not json"[..], false),
        Err(PermError::Import(_))
    ));
}

#[test]
fn the_sweeper_deletes_expired_nodes_as_the_system() {
    let authority = authority();
    let soon = unix_now() + 1;
    authority
        .apply(
            Actor::Cli,
            set(user(ALICE), perm("trial", true).expiring_at(soon)),
        )
        .unwrap();
    authority
        .apply(Actor::Cli, set(user(ALICE), perm("kept", true)))
        .unwrap();
    assert_eq!(authority.check(ALICE, "trial", &global()), Tristate::True);
    let mut notifications = authority.subscribe();
    assert_eq!(authority.sweep().unwrap().ops, 0, "nothing expired yet");
    std::thread::sleep(Duration::from_millis(1100));
    // Expiry is exact at check time, before any sweep.
    assert_eq!(
        authority.check(ALICE, "trial", &global()),
        Tristate::Undefined
    );
    let applied = authority.sweep().unwrap();
    assert_eq!(applied.ops, 1);
    let Ok(Notification::Committed { event, .. }) = notifications.try_recv() else {
        panic!("sweeps notify")
    };
    assert_eq!(event.actor, Actor::System);
    assert_eq!(event.reason.as_deref(), Some("expired"));
    assert!(matches!(
        &event.ops[0],
        Op::Node { before: Some(node), after: None, .. } if node.payload.key() == "trial"
    ));
    assert_eq!(authority.user(ALICE).unwrap().nodes, [perm("kept", true)]);
}

#[test]
fn registered_permissions_are_replaced_per_owner() {
    let authority = authority();
    let registered = |node: &str, plugin: Option<&str>| RegisteredPermission {
        node: node.into(),
        description: Some(format!("{node} help")),
        default: PermissionDefault::Op(None),
        plugin: plugin.map(str::to_owned),
    };
    assert_eq!(
        authority
            .register(
                "server.lobby",
                vec![
                    registered("Essentials.Fly", Some("Essentials")),
                    registered("lobby.menu", None),
                ],
            )
            .unwrap(),
        2
    );
    authority
        .register("server.survival", vec![registered("survival.claim", None)])
        .unwrap();
    authority
        .register("server.lobby", vec![registered("lobby.menu", None)])
        .unwrap();
    let all = authority.registered().unwrap();
    let nodes: Vec<(&str, &str, Option<&str>)> = all
        .iter()
        .map(|r| {
            (
                r.permission.node.as_str(),
                r.owner.as_str(),
                r.permission.plugin.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        nodes,
        [
            ("lobby.menu", "server.lobby", None),
            ("survival.claim", "server.survival", None)
        ]
    );
    assert!(authority.register("bad owner", vec![]).is_err());
    assert!(
        authority
            .register("server.lobby", vec![registered("bad node", None)])
            .is_err()
    );
    // The catalogue is not policy: no revision.
    assert_eq!(authority.revision(), 0);
}

#[test]
fn log_queries_filter_by_revision_actor_and_subject() {
    let authority = authority();
    authority.apply(Actor::Cli, create("vip", 0)).unwrap();
    authority
        .apply(Actor::Lua("join".into()), set(user(ALICE), parent("vip")))
        .unwrap();
    authority
        .apply(Actor::Cli, set(group("vip"), perm("fly", true)))
        .unwrap();
    authority
        .apply_with(
            Actor::Cli,
            set(user(BOB), perm("chat", true)),
            Some("welcome"),
            None,
        )
        .unwrap();
    let revisions = |query: LogQuery| -> Vec<u64> {
        authority
            .log(&query)
            .unwrap()
            .iter()
            .map(|e| e.revision)
            .collect()
    };
    assert_eq!(
        revisions(LogQuery {
            since: Some(2),
            ..LogQuery::default()
        }),
        [3, 4]
    );
    assert_eq!(
        revisions(LogQuery {
            actor: Some(Actor::Lua("join".into())),
            ..LogQuery::default()
        }),
        [2]
    );
    assert_eq!(
        revisions(LogQuery {
            subject: Some(group("vip")),
            ..LogQuery::default()
        }),
        [1, 3]
    );
    assert_eq!(
        revisions(LogQuery {
            subject: Some(user(ALICE)),
            ..LogQuery::default()
        }),
        [2]
    );
    assert_eq!(
        revisions(LogQuery {
            limit: 2,
            ..LogQuery::default()
        }),
        [3, 4]
    );
    let last = authority.log(&LogQuery::default()).unwrap().pop().unwrap();
    assert_eq!(last.reason.as_deref(), Some("welcome"));
    assert_eq!(last.changed, [BOB]);
    let (members, truncated) = authority.members(10).unwrap();
    assert!(!truncated);
    assert_eq!(
        members,
        [Member {
            uuid: ALICE,
            name: None,
            groups: vec!["vip".into()],
        }]
    );
}

#[test]
fn the_extension_store_upgrade_runs_once() {
    let scratch = Scratch::new("upgrade");
    let authority = scratch.open();
    let nodes = || {
        Ok(vec![
            (user(ALICE), parent("staff")),
            (group("default"), perm("chat.send", true)),
            (group("missing"), perm("skipped", true)),
        ])
    };
    let applied = authority.migrate_once(nodes).unwrap().unwrap();
    assert_eq!(applied.ops, 2, "a node of a missing group is skipped");
    assert_eq!(
        authority.log(&LogQuery::default()).unwrap()[0].actor,
        Actor::System
    );
    assert!(authority.migrate_once(|| unreachable!()).unwrap().is_none());
    drop(authority);
    let reopened = scratch.open();
    assert!(reopened.migrate_once(|| unreachable!()).unwrap().is_none());
    assert_eq!(
        reopened.check(ALICE, "chat.send", &global()),
        Tristate::True
    );
    assert_eq!(reopened.user(ALICE).unwrap().nodes, [parent("staff")]);
}

fn source(extension: &str) -> String {
    format!(
        "return {{ listeners = {{ public = '127.0.0.1:0' }}, backends = {{ lobby = '127.0.0.1:25001' }}, routes = {{ public = 'lobby' }}, authentication = {{ online_mode = true }}, forwarding = {{ mode = 'velocity', secret_env = 'TEST_SECRET' }}, extensions = {{ {extension} }} }}"
    )
}

fn runtime(script: &str) -> Extensions {
    let config = Config::from_lua(script, "permissions-test.lua").unwrap();
    Extensions::new(&config, Broker::new(Default::default()).unwrap(), None).unwrap()
}

fn context(id: u8) -> Context {
    Context::authenticated(
        u64::from(id),
        &crate::auth::AuthenticatedProfile {
            uuid: [id; 16],
            name: format!("Player{id}"),
            properties: vec![],
        },
    )
}

#[tokio::test]
async fn the_lua_module_reads_and_writes_through_the_authority() {
    let runtime = runtime(&source(
        r#"api_version = 3,
        groups = { default = { permissions = { ['perm.use'] = true } } },
        commands = { perm = { permission = 'perm.use', run = function(ctx)
            local perms = require('rift.permissions')
            local me = ctx.uuid
            assert(perms.check(me, 'perm.use') == true)
            assert(perms.check(me, 'nothing.here') == nil)
            local first = perms.user(me):set('fly', true, { contexts = { server = 'survival' }, reason = 'test' })
            assert(first.changed and first.revision > 0)
            assert(not perms.user(me):set('fly', true, { contexts = { server = 'survival' } }).changed)
            assert(perms.check(me, 'fly', { server = 'survival' }) == true)
            assert(perms.check(me, 'fly', {}) == nil)
            perms.user(me):set('chat.color', false)
            assert(ctx.has_permission('chat.color') == false)
            assert(perms.group('vip'):exists() == false)
            perms.group('vip'):create({ weight = 10, display_name = 'VIP' })
            perms.group('vip'):set_prefix(100, '&6[VIP] ')
            perms.group('vip'):set_meta('homes', 5)
            perms.user(me):add_parent('vip', { expires_in = 3600 })
            local meta = perms.meta(me, {})
            assert(meta.prefix == '&6[VIP] ' and meta.meta.homes == '5' and meta.primary_group == 'vip')
            local found = false
            for _, group in ipairs(perms.groups()) do
                if group.name == 'vip' then found = group.weight == 10 and group.display_name == 'VIP' end
            end
            assert(found)
            local compiled = perms.compiled(me)
            assert(compiled.uuid == me and #compiled.entries > 0 and compiled.prefix[1].t == '&6[VIP] ')
            perms.group('mod'):create()
            perms.track('ranks'):set({ 'default', 'vip', 'mod' })
            perms.track('ranks'):promote(me)
            local parents = {}
            for _, node in ipairs(perms.user(me):nodes()) do
                if node.kind == 'inherit' then parents[#parents + 1] = node.group end
            end
            assert(#parents == 1 and parents[1] == 'mod', table.concat(parents, ','))
            perms.user(me):unset('fly', { server = 'survival' })
            assert(perms.check(me, 'fly', { server = 'survival' }) == nil)
            assert(not rift.try(function() perms.group('default'):delete() end))
            assert(not rift.try(function() perms.user('nope'):set('a', true) end))
            assert(not rift.try(function() perms.has(me, 'a') end))
            return { message = 'ok' }
        end } }"#,
    ));
    let session = runtime.session(context(1)).unwrap();
    assert_eq!(
        session.command("perm").await.unwrap(),
        Action::Message("ok".into())
    );
    let log = runtime.permissions().log(&LogQuery::default()).unwrap();
    let first = log
        .iter()
        .find(|e| e.actor == Actor::Lua("command.perm".into()))
        .unwrap();
    assert_eq!(first.reason.as_deref(), Some("test"));
    assert!(runtime.permissions().group("vip").is_some());
}

#[tokio::test]
async fn change_handlers_and_context_providers_run_for_api_v3() {
    let script = format!(
        r#"rift.permissions.on_change(function(event)
            rift.publish('perm.events', event.subject .. ' ' .. event.kind .. ' ' .. event.key)
            -- Writes made here are not fed back to the handler.
            rift.permissions.user(string.rep('02', 16)):set('echo', true)
        end)
        rift.permissions.contexts.provide('region', function(player)
            if player.server == 'lobby' then return 'spawn' end
        end)
        {}"#,
        source("api_version = 3")
    );
    let runtime = runtime(&script);
    let broker = runtime.broker().clone();
    let mut events = broker.subscribe("perm.events", None).unwrap();
    let scheduler = runtime.scheduler();
    runtime
        .permissions()
        .apply(Actor::Cli, set(user(ALICE), perm("fly", true)))
        .unwrap();
    let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        &event.payload[..],
        format!("user:{ALICE} permission fly").as_bytes()
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(300), events.recv())
            .await
            .is_err(),
        "the handler's own write is not an event for it"
    );
    assert_eq!(
        runtime.permissions().check(BOB, "echo", &global()),
        Tristate::True
    );
    scheduler.shutdown().await;
    // Providers run when the player reaches a server.
    let (_handle, _actions) = runtime
        .players()
        .register(1, &[1; 16], "Player1", 774, "lobby");
    let mut session = runtime.session(context(1)).unwrap();
    session.reserve("lobby").unwrap();
    session.ready("lobby").await;
    assert_eq!(
        runtime.permissions().contexts(ALICE).to_string(),
        "region=spawn;server=lobby"
    );
    // API v2 has no rift.permissions functions at all.
    let v2 = self::runtime(&source(
        "api_version = 2, login = function(ctx) rift.permissions.check(ctx.uuid, 'a') end",
    ));
    let error = v2
        .session(context(1))
        .unwrap()
        .decision("login")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("API v3"), "{error}");
}

#[test]
fn the_configuration_mirror_replaces_only_config_rows_once_per_change() {
    let config = |groups: &str| {
        Config::from_lua(
            &source(&format!("api_version = 2, groups = {{ {groups} }}")),
            "mirror.lua",
        )
        .unwrap()
    };
    let first = config("vip = { weight = 5, prefix = '&6', permissions = { fly = true } }");
    let runtime = Extensions::new(&first, Broker::new(Default::default()).unwrap(), None).unwrap();
    let authority = runtime.permissions().clone();
    let revision = authority.revision();
    assert_eq!(revision, 1, "the first activation mirrors the groups");
    let vip = authority.group("vip").unwrap();
    assert_eq!(vip.weight, 5);
    assert!(
        vip.nodes
            .iter()
            .all(|n| n.source == rift_permissions::Source::Config)
    );
    authority
        .apply(Actor::Cli, set(group("vip"), perm("store.node", true)))
        .unwrap();
    let reload = |config: &Config, previous: &Extensions| {
        let next = Extensions::new(config, previous.broker().clone(), Some(previous)).unwrap();
        next.activate();
        next
    };
    let same = reload(&first, &runtime);
    assert_eq!(
        authority.revision(),
        revision + 1,
        "an identical reload records nothing"
    );
    let changed = reload(&config("vip = { permissions = { heal = true } }"), &same);
    assert_eq!(
        authority.revision(),
        revision + 2,
        "one revision per reload"
    );
    let vip = authority.group("vip").unwrap();
    let keys: Vec<String> = vip.nodes.iter().map(|n| n.payload.key()).collect();
    assert_eq!(keys, ["store.node", "heal"]);
    assert_eq!(vip.weight, 0);
    reload(&config(""), &changed);
    let vip = authority.group("vip").unwrap();
    assert_eq!(vip.nodes, [perm("store.node", true)]);
}

#[test]
fn removed_extension_permissions_are_rejected_with_a_pointer() {
    for version in [1, 2, 3] {
        let error = Config::from_lua(
            &source(&format!(
                "api_version = {version}, permissions = {{ ['*'] = {{ chat = true }} }}"
            )),
            "legacy.lua",
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("extensions.permissions was removed")
                && error.contains("docs/permissions.md"),
            "{error}"
        );
    }
}

#[test]
fn errors_map_onto_wire_codes() {
    use rift_permissions::wire::ErrorCode;
    assert_eq!(PermError::Deadline.code(), ErrorCode::Unavailable);
    assert_eq!(
        PermError::Forbidden("x".into()).code(),
        ErrorCode::Forbidden
    );
    assert_eq!(PermError::Storage("x".into()).code(), ErrorCode::Internal);
    let io: std::io::Error = PermError::Limit {
        what: "groups",
        limit: 1,
    }
    .into();
    assert_eq!(io.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn proxy_contexts_follow_the_session_dimension_and_game_mode_until_a_switch() {
    let authority = Arc::new(authority());
    let scoped = |key: &str, value: &str| ContextSet::of(key, value).unwrap();
    for (node, contexts) in [
        ("fly", scoped("gamemode", "creative")),
        ("portal", scoped("dimension", "minecraft:the_nether")),
    ] {
        authority
            .apply(
                Actor::Cli,
                set(
                    user(ALICE),
                    perm(node, true).with_contexts(contexts).unwrap(),
                ),
            )
            .unwrap();
    }
    let checks = |authority: &Authority| {
        (
            authority.check_active(ALICE, "fly"),
            authority.check_active(ALICE, "portal"),
        )
    };
    // Offline players have no contexts to update.
    authority.set_session_contexts(ALICE, Some("minecraft:the_nether"), Some("creative"));
    assert!(authority.contexts(ALICE).is_empty());

    authority.join(ALICE, "Alice", "lobby", None);
    assert_eq!(
        checks(&authority),
        (Tristate::Undefined, Tristate::Undefined)
    );
    authority.set_session_contexts(ALICE, Some("minecraft:the_nether"), Some("creative"));
    assert_eq!(
        authority.contexts(ALICE).to_string(),
        "dimension=minecraft:the_nether;gamemode=creative;server=lobby"
    );
    assert_eq!(checks(&authority), (Tristate::True, Tristate::True));
    // A game mode change keeps the dimension, and the reverse.
    authority.set_session_contexts(ALICE, Some("minecraft:the_nether"), Some("survival"));
    assert_eq!(checks(&authority), (Tristate::Undefined, Tristate::True));
    authority.set_session_contexts(ALICE, Some("minecraft:overworld"), Some("creative"));
    assert_eq!(checks(&authority), (Tristate::True, Tristate::Undefined));
    // Backend-reported `world` and custom contexts are left alone; a value
    // the context syntax refuses is left out.
    authority
        .set_proxy_context(ALICE, "world", Some("world_nether"))
        .unwrap();
    authority.set_session_contexts(ALICE, Some("bad;key"), None);
    assert_eq!(
        authority.contexts(ALICE).to_string(),
        "server=lobby;world=world_nether"
    );

    // A switch restarts from the server; the new world's packets refill them.
    authority.set_session_contexts(ALICE, Some("minecraft:the_nether"), Some("creative"));
    authority.switch_server(ALICE, "survival", None);
    assert_eq!(authority.contexts(ALICE).to_string(), "server=survival");
    assert_eq!(
        checks(&authority),
        (Tristate::Undefined, Tristate::Undefined)
    );
    authority.set_session_contexts(ALICE, Some("minecraft:the_nether"), Some("creative"));
    assert_eq!(checks(&authority), (Tristate::True, Tristate::True));
    assert_eq!(authority.server_of(ALICE).as_deref(), Some("survival"));
    authority.leave(ALICE);
    assert!(authority.contexts(ALICE).is_empty());
}

#[test]
fn the_group_context_applies_on_every_instance_of_a_service_group() {
    let authority = Arc::new(authority());
    let scoped = |key: &str, value: &str| ContextSet::of(key, value).unwrap();
    for (node, contexts) in [
        ("lobby.fly", scoped("group", "lobby")),
        ("lobby.old", scoped("server", "lobby")),
    ] {
        authority
            .apply(
                Actor::Cli,
                set(
                    user(ALICE),
                    perm(node, true).with_contexts(contexts).unwrap(),
                ),
            )
            .unwrap();
    }
    authority.join(ALICE, "Alice", "lobby-2", Some("lobby"));
    assert_eq!(
        authority.contexts(ALICE).to_string(),
        "group=lobby;server=lobby-2"
    );
    assert_eq!(authority.check_active(ALICE, "lobby.fly"), Tristate::True);
    // The instance is not called `lobby`, so `server=lobby` never matches.
    assert_eq!(
        authority.check_active(ALICE, "lobby.old"),
        Tristate::Undefined
    );
    // The group follows the server and goes away on a backend outside groups.
    authority.switch_server(ALICE, "lobby-7", Some("lobby"));
    assert_eq!(authority.check_active(ALICE, "lobby.fly"), Tristate::True);
    authority.switch_server(ALICE, "survival", None);
    assert_eq!(authority.contexts(ALICE).to_string(), "server=survival");
    assert_eq!(
        authority.check_active(ALICE, "lobby.fly"),
        Tristate::Undefined
    );
    // Like `server`, it is filled by Rift: it cannot be reported or provided.
    assert!(
        authority
            .set_proxy_context(ALICE, "group", Some("lobby"))
            .is_ok()
    );
    assert!(
        crate::config::Config::from_lua(
            "rift.config.listeners.p='127.0.0.1:0' rift.config.backends.b='127.0.0.1:1' rift.config.routes.p='b' rift.config.permissions={contexts={external={'group'}}}",
            "external.lua",
        )
        .unwrap_err()
        .to_string()
        .contains("\"group\" is filled by Rift")
    );
}

#[test]
fn luckperms_server_contexts_naming_a_service_group_become_group_contexts() {
    let authority = authority();
    authority.set_service_groups(&["Lobby".to_owned(), "bedwars".to_owned()]);
    let export = serde_json::json!({
        "metadata": {},
        "groups": {"vip": {"nodes": [
            {"key": "lobby.fly", "value": true, "context": {"server": ["lobby", "survival"]}},
            {"key": "bedwars.kit", "value": true, "context": {"server": "bedwars", "world": "arena"}},
        ]}},
        "users": {},
        "tracks": {},
    });
    authority
        .import_luckperms(Actor::Import, export.to_string().as_bytes(), false)
        .unwrap();
    let nodes: Vec<String> = authority
        .group("vip")
        .unwrap()
        .nodes
        .iter()
        .map(|node| format!("{} {}", node.payload.key(), node.contexts))
        .collect();
    assert!(
        nodes.contains(&"lobby.fly group=lobby".to_owned()),
        "{nodes:?}"
    );
    // `survival` is a fixed server, not a group.
    assert!(
        nodes.contains(&"lobby.fly server=survival".to_owned()),
        "{nodes:?}"
    );
    assert!(
        nodes.contains(&"bedwars.kit group=bedwars;world=arena".to_owned()),
        "{nodes:?}"
    );
    // Exported, `group=lobby` reads back in LuckPerms as `server=lobby`.
    let exported = serde_json::to_value(authority.export_luckperms().unwrap()).unwrap();
    let contexts: Vec<&serde_json::Value> = exported["groups"]["vip"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|node| node["key"] == "lobby.fly")
        .map(|node| &node["context"])
        .collect();
    assert!(
        contexts.contains(&&serde_json::json!({"server": "lobby"})),
        "{contexts:?}"
    );
}
