use std::sync::{Arc, Mutex};

use rift_permissions::{
    Change, ChangeEvent, ContextSet, Node, NodeKind, SubjectRef, Tristate, Uuid,
    change::{Filter, Patch},
    wire::ErrorCode,
};
use rift_permissions_client::{Error, RiftPermissions};
use rift_permissions_testing::TestPermissions;

const ALICE: Uuid = Uuid::from_u128(1);
const BOB: Uuid = Uuid::from_u128(2);

fn fixture() -> TestPermissions {
    TestPermissions::builder()
        .group("default", |g| g.permission("spawn", true))
        .group("vip", |g| {
            g.weight(10)
                .display_name("VIP")
                .permission("essentials.fly", true)
                .prefix(100, "[VIP] ")
                .meta("homes", "3")
        })
        .group("mod", |g| {
            g.weight(20)
                .parent("vip")
                .permission("essentials.fly", false)
                .meta("homes", "10")
        })
        .user(ALICE, |u| {
            u.parent("vip").permission("essentials.home", true)
        })
        .user(BOB, |u| {
            u.parent("mod").node(
                Node::permission("build", true)
                    .unwrap()
                    .with_context("server", "creative")
                    .unwrap(),
            )
        })
        .track("ranks", &["default", "vip", "mod"])
        .server("survival")
        .build()
}

fn global() -> ContextSet {
    ContextSet::new()
}

#[test]
fn builder_compiles_like_the_proxy() {
    let perms = fixture();
    let dynamic: &dyn RiftPermissions = &perms;
    assert_eq!(dynamic.revision(), 1);
    assert_eq!(
        dynamic.check(ALICE, "essentials.fly", &global()),
        Tristate::True
    );
    assert_eq!(dynamic.check(ALICE, "spawn", &global()), Tristate::True);
    assert_eq!(dynamic.check(ALICE, "group.vip", &global()), Tristate::True);
    assert_eq!(
        dynamic.check(ALICE, "other", &global()),
        Tristate::Undefined
    );
    // Heavier groups rank first, so mod's deny wins over vip's grant.
    assert_eq!(
        dynamic.check(BOB, "essentials.fly", &global()),
        Tristate::False
    );
    let meta = dynamic.meta(BOB, &global()).unwrap();
    assert_eq!(meta.meta["homes"], "10");
    assert_eq!(meta.prefix.as_deref(), Some("[VIP] "));
    assert_eq!(dynamic.primary_group(BOB).as_deref(), Some("mod"));
    assert_eq!(dynamic.groups(BOB, &global()), ["mod", "vip", "default"]);
    // The builder's server fills in the `server` context.
    assert_eq!(dynamic.check(BOB, "build", &global()), Tristate::Undefined);
    let creative = ContextSet::of("server", "creative").unwrap();
    assert_eq!(dynamic.check(BOB, "build", &creative), Tristate::True);
    let names: Vec<_> = dynamic
        .group_list()
        .iter()
        .map(|g| g.name.clone())
        .collect();
    assert_eq!(names, ["default", "mod", "vip"]);
    assert_eq!(dynamic.group_list()[2].display_name.as_deref(), Some("VIP"));
    // Every player compiles, as on the proxy.
    let stranger = Uuid::from_u128(99);
    assert_eq!(dynamic.check(stranger, "spawn", &global()), Tristate::True);
    assert_eq!(perms.compiled(stranger).primary_group, "default");
}

#[test]
fn changes_bump_the_revision_and_notify_listeners() {
    let perms = fixture();
    let events: Arc<Mutex<Vec<ChangeEvent>>> = Arc::default();
    let sink = events.clone();
    let listener = perms.on_change(Box::new(move |event| {
        sink.lock().unwrap().push(event.clone())
    }));

    let deny = Change::SetNode {
        subject: SubjectRef::User(ALICE),
        node: Node::permission("Essentials.Fly", false).unwrap(),
    };
    let applied = perms.apply_now(Some(BOB), deny.clone()).unwrap();
    assert_eq!((applied.revision, applied.ops), (2, 1));
    assert_eq!(applied.changed_users, [ALICE]);
    assert_eq!(
        perms.check(ALICE, "essentials.fly", &global()),
        Tristate::False
    );
    // The same change again does nothing and creates no revision.
    let again = perms.apply_now(None, deny).unwrap();
    assert_eq!((again.revision, again.ops), (2, 0));

    // A group change reaches every member.
    let weight = Change::SetWeight {
        group: "vip".into(),
        weight: 30,
    };
    let applied = perms.apply_now(None, weight).unwrap();
    assert_eq!(applied.changed_groups, ["vip"]);
    assert_eq!(applied.changed_users, [BOB]);
    assert_eq!(
        perms.check(BOB, "essentials.fly", &global()),
        Tristate::True
    );

    {
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].actor.to_string(), format!("uuid:{BOB}"));
        assert_eq!(events[1].actor.to_string(), "backend:test");
        assert_eq!(events[1].groups, ["vip"]);
    }
    drop(listener);
    perms
        .apply_now(
            None,
            Change::CreateGroup {
                name: "builder".into(),
                weight: 0,
                display_name: None,
            },
        )
        .unwrap();
    assert_eq!(events.lock().unwrap().len(), 2);
}

#[test]
fn tracks_bulk_edits_and_batches() {
    let perms = fixture();
    let promote = |user| Change::Promote {
        user,
        track: "ranks".into(),
        contexts: global(),
    };
    perms.apply_now(None, promote(ALICE)).unwrap();
    assert_eq!(perms.primary_group(ALICE).as_deref(), Some("mod"));
    let Err(Error::Rejected(reply)) = perms.apply_now(None, promote(ALICE)) else {
        panic!("mod is the last group");
    };
    assert_eq!(reply.code, ErrorCode::Conflict);
    let demote = Change::Demote {
        user: ALICE,
        track: "ranks".into(),
        contexts: global(),
    };
    perms.apply_now(None, demote).unwrap();
    assert_eq!(perms.groups(ALICE, &global()), ["vip", "default"]);

    let bulk = Change::Bulk {
        filter: Filter {
            kind: Some(NodeKind::Permission),
            key_like: Some("essentials.%".into()),
            ..Filter::default()
        },
        patch: Patch {
            value: Some(false),
            ..Patch::default()
        },
    };
    // Alice's and vip's grants flip; mod's deny already matches the patch.
    let applied = perms.apply_now(None, bulk).unwrap();
    assert_eq!(applied.ops, 2);
    assert_eq!(
        perms.check(ALICE, "essentials.home", &global()),
        Tristate::False
    );

    // A failing batch changes nothing.
    let revision = perms.revision();
    let batch = Change::Batch {
        changes: vec![
            Change::SetPrimaryGroup {
                user: ALICE,
                group: Some("vip".into()),
            },
            Change::DeleteGroup {
                name: "nonexistent".into(),
            },
        ],
    };
    let Err(Error::Rejected(reply)) = perms.apply_now(None, batch) else {
        panic!("the batch names an unknown group");
    };
    assert_eq!(reply.code, ErrorCode::NotFound);
    assert_eq!(perms.revision(), revision);
    let invalid = Change::DeleteGroup {
        name: "default".into(),
    };
    let Err(Error::Rejected(reply)) = perms.apply_now(None, invalid) else {
        panic!("default cannot be deleted");
    };
    assert_eq!(reply.code, ErrorCode::InvalidRequest);
}

#[tokio::test]
async fn the_trait_applies_asynchronously() {
    let perms = TestPermissions::new();
    let grant = Change::SetNode {
        subject: SubjectRef::Group("default".into()),
        node: Node::permission("*", true).unwrap(),
    };
    let applied = perms.apply(Some(ALICE), grant).await.unwrap();
    assert_eq!(applied.revision, 2);
    assert_eq!(perms.check(BOB, "anything", &global()), Tristate::True);
}
