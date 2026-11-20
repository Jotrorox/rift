//! Inventory menus with a scripted client and backend (docs/menus.md).
use super::*;
use crate::{
    dialog::Owner,
    menu::{Click, CloseReason, Delivery, EventKind, Menu, Shown},
    protocol::menu as packets,
};
use rift_translation::{
    Direction as Wire,
    codec::item::window::{self, ClickSlot},
    wire::{Reader as WireReader, Writer},
};
use serde_json::json;

const CB: Wire = Wire::Clientbound;
const SB: Wire = Wire::Serverbound;
/// The first container ID Rift gives a menu.
const FIRST: i32 = packets::FIRST_CONTAINER;

fn version(protocol: i32) -> ProtocolVersion {
    ProtocolVersion::new(protocol).unwrap()
}

fn id(protocol: i32, direction: Wire, name: &str) -> i32 {
    rift_translation::data::version(protocol)
        .packet_id(rift_translation::State::Play, direction, name)
        .unwrap()
}

/// A session in play on `protocol` with an untranslated backend, as after
/// a join.
async fn menu_session(
    protocol: i32,
) -> (
    Session<DuplexStream, DuplexStream>,
    DuplexStream,
    DuplexStream,
) {
    let (mut client, input) = tokio::io::duplex(1 << 20);
    let handshake = Handshake {
        protocol,
        address: "play.test".into(),
        port: 25565,
        next_state: NextState::Login,
    };
    Codec::default()
        .write(&mut client, &handshake.packet())
        .await
        .unwrap();
    let mut session = Session::accept(input).await.unwrap();
    session.client.state = ConnectionState::new(State::Play);
    let (server, backend) = tokio::io::duplex(1 << 20);
    session.backend = Some(Connection::new(server, State::Play));
    session.joined = true;
    (session, client, backend)
}

async fn next(stream: &mut DuplexStream) -> Packet {
    receive(stream, Codec::default()).await
}

async fn nothing(stream: &mut DuplexStream) {
    assert!(
        timeout(Duration::from_millis(20), next(stream))
            .await
            .is_err(),
        "expected nothing"
    );
}

/// Read everything that arrives until the stream is quiet.
async fn drain(stream: &mut DuplexStream) -> Vec<Packet> {
    let mut packets = Vec::new();
    while let Ok(packet) = timeout(Duration::from_millis(20), next(stream)).await {
        packets.push(packet);
    }
    packets
}

/// Write what the session queued for both peers.
async fn flush(session: &mut Session<DuplexStream, DuplexStream>) {
    session.flush_injected().await.unwrap();
    if let Some(backend) = session.backend.as_mut() {
        poll_fn(|cx| backend.poll_replies(cx)).await.unwrap();
    }
}

async fn send(stream: &mut DuplexStream, packet: &Packet) {
    Codec::default().write(stream, packet).await.unwrap();
}

fn owner() -> Owner {
    Owner::Plugin("shop".into())
}

/// Slots: `#` decoration around the edge, `B` (11) reports the mouse
/// clicks with a payload, `D` (13) every kind, `A` (15) the mouse clicks,
/// `X` (22) closes without an id.
fn shop() -> serde_json::Value {
    json!({
        "id": "shop",
        "title": "Shop",
        "state": {"page": 2},
        "layout": ["#########", "# B D A #", "####X####"],
        "keys": {
            "#": {"item": "gray_stained_glass_pane", "name": " "},
            "B": {"item": "diamond", "id": "buy", "payload": {"price": 5}},
            "D": {"item": "dirt", "id": "dig", "clicks": "all"},
            "A": {"item": "apple", "id": "eat"},
            "X": {"item": "barrier", "close": true},
        },
    })
}
const SHOP_FILLED: usize = 23;
const BUY: i16 = 11;
const DIG: i16 = 13;
const CLOSE: i16 = 22;

fn hopper(name: &str) -> serde_json::Value {
    json!({"id": name, "type": "hopper", "slots": [{"slot": 2, "item": "stone", "id": "go"}]})
}

fn shown(spec: serde_json::Value, owner: Owner) -> Shown {
    Shown {
        menu: Arc::new(Menu::parse(&spec).unwrap()),
        owner,
        heads: Arc::default(),
    }
}

/// Show a menu and read what the client gets: `open_screen`, then one slot
/// per filled slot.
async fn show(
    session: &mut Session<DuplexStream, DuplexStream>,
    client: &mut DuplexStream,
    spec: serde_json::Value,
    owner: Owner,
) -> Vec<Packet> {
    let protocol = session.handshake.protocol;
    session.show_menu(shown(spec, owner)).await.unwrap();
    let packets = drain(client).await;
    assert_eq!(packets[0].id, id(protocol, CB, "open_screen"));
    packets
}

fn click_on(
    protocol: i32,
    container: i32,
    slot: i16,
    button: i8,
    mode: i32,
    changed: &[i16],
) -> Packet {
    let empty = if window::hashed_clicks(protocol) {
        ClickSlot::Hashed(None)
    } else {
        ClickSlot::Full(None)
    };
    let click = window::Click {
        container,
        state_id: 1,
        slot,
        button,
        action: 1,
        mode,
        clicked: None,
        changed: changed.iter().map(|slot| (*slot, empty.clone())).collect(),
        carried: empty,
    };
    let mut w = Writer::new();
    click.write(protocol, &mut w).unwrap();
    Packet::new(id(protocol, SB, "container_click"), w.into_inner())
}

fn click(protocol: i32, slot: i16, button: i8, mode: i32, changed: &[i16]) -> Packet {
    click_on(protocol, FIRST, slot, button, mode, changed)
}

/// (container, slot, whether it holds an item) of a `container_set_slot`.
fn slot_of(protocol: i32, packet: &Packet) -> (i32, i16, bool) {
    assert_eq!(packet.id, id(protocol, CB, "container_set_slot"));
    let mut r = WireReader::new(&packet.data);
    let slot = window::SetSlot::read(protocol, &mut r).unwrap();
    r.finish().unwrap();
    (slot.container, slot.slot, slot.item.is_some())
}

fn assert_empty_cursor(protocol: i32, packet: &Packet) {
    if protocol >= 768 {
        assert_eq!(packet.id, id(protocol, CB, "set_cursor_item"));
        assert_eq!(packet.data, [0]);
    } else {
        assert_eq!(slot_of(protocol, packet), (-1, -1, false));
    }
}

/// The client gets the slots back, then an empty cursor.
async fn assert_undone(protocol: i32, client: &mut DuplexStream, container: i32, slots: &[i16]) {
    let packets = drain(client).await;
    assert_eq!(packets.len(), slots.len() + 1, "{packets:?}");
    for (packet, slot) in packets.iter().zip(slots) {
        assert_eq!(slot_of(protocol, packet).0, container);
        assert_eq!(slot_of(protocol, packet).1, *slot);
    }
    assert_empty_cursor(protocol, packets.last().unwrap());
}

fn inventory(protocol: i32) -> Packet {
    let content = window::SetContent {
        container: 0,
        state_id: 1,
        items: vec![None; 46],
        carried: None,
    };
    let mut w = Writer::new();
    content.write(protocol, &mut w).unwrap();
    Packet::new(id(protocol, CB, "container_set_content"), w.into_inner())
}

fn hotbar_slot(protocol: i32) -> Packet {
    packets::set_slot(version(protocol), 0, 1, 36, &[0]).unwrap()
}

fn delivery(event: SessionEvent) -> Delivery {
    match event {
        SessionEvent::Menu(delivery) => delivery,
        other => panic!("expected a menu event, got {other:?}"),
    }
}

fn closes(session: &mut Session<DuplexStream, DuplexStream>) -> Vec<(String, CloseReason)> {
    session
        .take_menu_events()
        .into_iter()
        .map(|delivery| {
            assert_eq!(delivery.event.kind, EventKind::Close);
            (delivery.event.menu, delivery.event.reason.unwrap())
        })
        .collect()
}

#[tokio::test]
async fn a_menu_opens_with_its_slots_and_reports_a_click_to_its_owner() {
    timeout(Duration::from_secs(5), async {
        let protocol = 774;
        let (mut session, mut client, mut backend) = menu_session(protocol).await;
        let packets = show(&mut session, &mut client, shop(), owner()).await;
        let menu = Menu::parse(&shop()).unwrap();
        assert_eq!(
            packets[0],
            packets::open_screen(version(protocol), FIRST, menu.kind(), menu.title()).unwrap()
        );
        assert_eq!(packets.len(), 1 + SHOP_FILLED);
        let filled: Vec<i16> = packets[1..]
            .iter()
            .map(|packet| {
                let (container, slot, item) = slot_of(protocol, packet);
                assert_eq!(container, FIRST);
                assert!(item);
                slot
            })
            .collect();
        assert!(filled.contains(&BUY) && filled.contains(&CLOSE) && !filled.contains(&10));
        assert!(session.menu_open());

        send(&mut client, &click(protocol, BUY, 0, 0, &[BUY])).await;
        let delivery = delivery(session.forward().await.unwrap());
        assert_eq!(delivery.owner, owner());
        let event = delivery.event;
        assert_eq!(
            (event.menu.as_str(), event.kind, event.action.as_deref()),
            ("shop", EventKind::Click, Some("buy"))
        );
        assert_eq!(
            (event.slot, event.click, event.key),
            (Some(11), Some(Click::Left), None)
        );
        assert_eq!(event.payload, Some(json!({"price": 5})));
        assert_eq!(event.state, Some(json!({"page": 2})));
        assert_eq!((event.text, event.reason), (None, None));
        flush(&mut session).await;
        assert_undone(protocol, &mut client, FIRST, &[BUY]).await;
        // Nothing of Rift's container reached the backend.
        nothing(&mut backend).await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn decoration_and_unreported_kinds_are_undone_and_reported_to_nobody() {
    timeout(Duration::from_secs(5), async {
        let protocol = 774;
        let (mut session, mut client, mut backend) = menu_session(protocol).await;
        show(&mut session, &mut client, shop(), owner()).await;
        // Decoration.
        send(&mut client, &click(protocol, 0, 0, 0, &[0])).await;
        assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
        flush(&mut session).await;
        assert_undone(protocol, &mut client, FIRST, &[0]).await;
        // Q over a button that reports the mouse clicks only.
        send(&mut client, &click(protocol, BUY, 0, 4, &[BUY])).await;
        assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
        flush(&mut session).await;
        assert_undone(protocol, &mut client, FIRST, &[BUY]).await;
        // A drag over two buttons: both slots come back, nothing reported.
        send(&mut client, &click(protocol, -999, 1, 5, &[BUY, DIG])).await;
        assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
        flush(&mut session).await;
        assert_undone(protocol, &mut client, FIRST, &[BUY, DIG]).await;
        // An empty slot, and a click outside the window.
        send(&mut client, &click(protocol, 10, 0, 0, &[10])).await;
        assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
        send(&mut client, &click(protocol, -999, 0, 0, &[])).await;
        assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
        flush(&mut session).await;
        drain(&mut client).await;
        // A button with every kind reports the drop, and Ctrl+Q.
        for (button, kind) in [(0, Click::Drop), (1, Click::DropAll)] {
            send(&mut client, &click(protocol, DIG, button, 4, &[DIG])).await;
            let event = delivery(session.forward().await.unwrap()).event;
            assert_eq!(
                (event.action.as_deref(), event.click),
                (Some("dig"), Some(kind))
            );
        }
        assert!(session.take_menu_events().is_empty());
        flush(&mut session).await;
        nothing(&mut backend).await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn clicks_before_1_17_are_undone_from_the_click_alone() {
    timeout(Duration::from_secs(5), async {
        let protocol = 340;
        let (mut session, mut client, mut backend) = menu_session(protocol).await;
        let packets = show(&mut session, &mut client, shop(), owner()).await;
        assert_eq!(packets.len(), 1 + SHOP_FILLED);
        // A left click: the clicked slot and the cursor (container -1).
        send(&mut client, &click(protocol, BUY, 0, 0, &[])).await;
        let event = delivery(session.forward().await.unwrap()).event;
        assert_eq!(event.click, Some(Click::Left));
        flush(&mut session).await;
        assert_undone(protocol, &mut client, FIRST, &[BUY]).await;
        nothing(&mut backend).await;
        // A double click's collection: every menu slot, and the inventory
        // rows are asked for again.
        send(&mut client, &click(protocol, BUY, 0, 6, &[])).await;
        assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
        flush(&mut session).await;
        let all: Vec<i16> = (0..27).collect();
        assert_undone(protocol, &mut client, FIRST, &all).await;
        assert_eq!(
            next(&mut backend).await,
            packets::inventory_request(protocol).unwrap()
        );
        // F before 1.16 is no click kind; a number key is.
        send(&mut client, &click(protocol, DIG, 2, 2, &[])).await;
        let event = delivery(session.forward().await.unwrap()).event;
        assert_eq!((event.click, event.key), (Some(Click::NumberKey), Some(3)));
        flush(&mut session).await;
        assert_undone(protocol, &mut client, FIRST, &[DIG]).await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn clicks_from_1_17_are_undone_from_their_list_of_changed_slots() {
    timeout(Duration::from_secs(5), async {
        for protocol in [756, 767, 774] {
            let (mut session, mut client, mut backend) = menu_session(protocol).await;
            show(&mut session, &mut client, shop(), owner()).await;
            // A number key over a button: its slot, a hotbar slot (54) and
            // the cursor.
            send(&mut client, &click(protocol, DIG, 3, 2, &[DIG, 54])).await;
            let event = delivery(session.forward().await.unwrap()).event;
            assert_eq!((event.click, event.key), (Some(Click::NumberKey), Some(4)));
            flush(&mut session).await;
            assert_undone(protocol, &mut client, FIRST, &[DIG]).await;
            assert_eq!(
                next(&mut backend).await,
                packets::inventory_request(protocol).unwrap(),
                "{protocol}"
            );
            // F swaps with the off hand, which the list never names.
            send(&mut backend, &inventory(protocol)).await;
            session.forward().await.unwrap();
            drain(&mut client).await;
            send(&mut client, &click(protocol, DIG, 40, 2, &[DIG])).await;
            let event = delivery(session.forward().await.unwrap()).event;
            assert_eq!(event.click, Some(Click::Offhand));
            flush(&mut session).await;
            assert_undone(protocol, &mut client, FIRST, &[DIG]).await;
            assert_eq!(
                next(&mut backend).await,
                packets::inventory_request(protocol).unwrap()
            );
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn nothing_of_rift_s_containers_reaches_the_backend_even_late() {
    timeout(Duration::from_secs(5), async {
        let protocol = 774;
        let (mut session, mut client, mut backend) = menu_session(protocol).await;
        show(&mut session, &mut client, shop(), owner()).await;
        // A button click on Rift's container is not the backend's.
        let button = Packet::new(
            id(protocol, SB, "container_button_click"),
            vec![FIRST as u8, 0],
        );
        send(&mut client, &button).await;
        assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
        // Rift closes the menu; a click that crossed the close is dropped.
        session.close_menu(&owner(), Some("shop")).await.unwrap();
        assert_eq!(
            next(&mut client).await,
            packets::close(version(protocol), FIRST).unwrap()
        );
        send(&mut client, &click(protocol, BUY, 0, 0, &[BUY])).await;
        assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
        send(
            &mut client,
            &packets::client_close(version(protocol), FIRST).unwrap(),
        )
        .await;
        assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
        flush(&mut session).await;
        nothing(&mut backend).await;
        nothing(&mut client).await;
        // A late shift-click moved the player's own items: they are asked
        // for again.
        send(&mut client, &click(protocol, BUY, 0, 1, &[BUY, 40])).await;
        assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
        flush(&mut session).await;
        assert_eq!(
            next(&mut backend).await,
            packets::inventory_request(protocol).unwrap()
        );
        // The backend's own containers stay the backend's.
        let theirs = click_on(protocol, 3, 0, 0, 0, &[0]);
        send(&mut client, &theirs).await;
        session.forward().await.unwrap();
        assert_eq!(next(&mut backend).await, theirs);
        assert_eq!(closes(&mut session), [("shop".into(), CloseReason::Plugin)]);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn backends_up_to_1_16_5_are_asked_with_a_rejected_click_rift_acknowledges() {
    timeout(Duration::from_secs(5), async {
        for protocol in [340, 754] {
            let (mut session, mut client, mut backend) = menu_session(protocol).await;
            show(&mut session, &mut client, shop(), owner()).await;
            // A shift-click on a button is reported, and moved items into the
            // inventory rows on the client.
            send(&mut client, &click(protocol, BUY, 0, 1, &[])).await;
            let event = delivery(session.forward().await.unwrap()).event;
            assert_eq!(event.click, Some(Click::ShiftLeft));
            flush(&mut session).await;
            assert_eq!(
                next(&mut backend).await,
                packets::inventory_request(protocol).unwrap()
            );
            // One request at a time: another click waits for the answer.
            send(&mut client, &click(protocol, 30, 0, 0, &[])).await;
            session.forward().await.unwrap();
            flush(&mut session).await;
            nothing(&mut backend).await;
            drain(&mut client).await;
            // The rejection is Rift's: acknowledged at once, never passed on.
            let rejection = Packet::new(id(protocol, CB, "container_ack"), vec![0, 0xff, 0xff, 0]);
            send(&mut backend, &rejection).await;
            send(&mut backend, &inventory(protocol)).await;
            assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
            assert_eq!(next(&mut client).await, inventory(protocol));
            assert_eq!(
                next(&mut backend).await,
                packets::acknowledgement(protocol).unwrap()
            );
            // The answer came: the click that waited asks once more.
            flush(&mut session).await;
            assert_eq!(
                next(&mut backend).await,
                packets::inventory_request(protocol).unwrap()
            );
            // That request's rejection is consumed too; one Rift did not
            // cause passes on.
            send(&mut backend, &rejection).await;
            send(&mut backend, &inventory(protocol)).await;
            session.forward().await.unwrap();
            send(&mut backend, &rejection).await;
            session.forward().await.unwrap();
            flush(&mut session).await;
            let passed = drain(&mut client).await;
            assert_eq!(passed, [inventory(protocol), rejection]);
            assert_eq!(
                next(&mut backend).await,
                packets::acknowledgement(protocol).unwrap()
            );
            nothing(&mut backend).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_1_17_backend_is_asked_when_the_menu_closes() {
    timeout(Duration::from_secs(5), async {
        let protocol = 755;
        let (mut session, mut client, mut backend) = menu_session(protocol).await;
        show(&mut session, &mut client, shop(), owner()).await;
        send(&mut client, &click(protocol, BUY, 0, 1, &[BUY, 40])).await;
        delivery(session.forward().await.unwrap());
        flush(&mut session).await;
        // It would answer slot by slot, which the client drops meanwhile.
        nothing(&mut backend).await;
        drain(&mut client).await;
        send(
            &mut client,
            &packets::client_close(version(protocol), FIRST).unwrap(),
        )
        .await;
        session.forward().await.unwrap();
        flush(&mut session).await;
        assert_eq!(
            next(&mut backend).await,
            packets::inventory_request(protocol).unwrap()
        );
        assert_eq!(closes(&mut session), [("shop".into(), CloseReason::Player)]);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_single_slot_update_during_a_menu_is_asked_for_again_on_old_clients() {
    timeout(Duration::from_secs(5), async {
        for (protocol, asked) in [(767, true), (340, true), (768, false), (774, false)] {
            let (mut session, mut client, mut backend) = menu_session(protocol).await;
            show(&mut session, &mut client, shop(), owner()).await;
            // The backend gives an item: passed on, and dropped by a client
            // before 1.21.2 except for the hotbar.
            send(&mut backend, &hotbar_slot(protocol)).await;
            session.forward().await.unwrap();
            assert_eq!(next(&mut client).await, hotbar_slot(protocol));
            nothing(&mut backend).await;
            session.close_menu(&owner(), None).await.unwrap();
            flush(&mut session).await;
            if asked {
                assert_eq!(
                    next(&mut backend).await,
                    packets::inventory_request(protocol).unwrap(),
                    "{protocol}"
                );
            }
            nothing(&mut backend).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn creative_slots_are_dropped_while_a_menu_is_open() {
    timeout(Duration::from_secs(5), async {
        let protocol = 774;
        let (mut session, mut client, mut backend) = menu_session(protocol).await;
        let creative = Packet::new(id(protocol, SB, "set_creative_mode_slot"), vec![0, 36, 0]);
        show(&mut session, &mut client, shop(), owner()).await;
        send(&mut client, &creative).await;
        assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
        nothing(&mut backend).await;
        session.close_menu(&owner(), None).await.unwrap();
        send(&mut client, &creative).await;
        session.forward().await.unwrap();
        assert_eq!(next(&mut backend).await, creative);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn every_ending_has_its_reason() {
    timeout(Duration::from_secs(5), async {
        let protocol = 774;
        let v = version(protocol);
        let (mut session, mut client, mut backend) = menu_session(protocol).await;
        let mut container = FIRST;

        // The player presses Escape.
        show(&mut session, &mut client, shop(), owner()).await;
        send(&mut client, &packets::client_close(v, container).unwrap()).await;
        assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
        assert_eq!(closes(&mut session), [("shop".into(), CloseReason::Player)]);
        assert!(!session.menu_open());

        // A button with close. A click on the previous menu is dropped.
        show(&mut session, &mut client, shop(), owner()).await;
        container += 1;
        send(&mut client, &click(protocol, CLOSE, 0, 0, &[CLOSE])).await;
        assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
        assert!(session.menu_open());
        let close = click_on(protocol, container, CLOSE, 0, 0, &[CLOSE]);
        send(&mut client, &close).await;
        assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
        flush(&mut session).await;
        let packets = drain(&mut client).await;
        assert_eq!(packets.last(), Some(&packets::close(v, container).unwrap()));
        assert_eq!(closes(&mut session), [("shop".into(), CloseReason::Plugin)]);

        // close_menu names the menu of its owner, or any menu.
        show(&mut session, &mut client, shop(), owner()).await;
        container += 1;
        session.close_menu(&owner(), Some("other")).await.unwrap();
        session
            .close_menu(&Owner::Script, Some("shop"))
            .await
            .unwrap();
        assert!(session.menu_open());
        session.close_menu(&Owner::Script, None).await.unwrap();
        assert_eq!(
            next(&mut client).await,
            packets::close(v, container).unwrap()
        );
        assert_eq!(closes(&mut session), [("shop".into(), CloseReason::Plugin)]);

        // Another menu replaces it; the same menu again redraws it.
        show(&mut session, &mut client, shop(), owner()).await;
        show(&mut session, &mut client, hopper("other"), owner()).await;
        assert_eq!(
            closes(&mut session),
            [("shop".into(), CloseReason::Replaced)]
        );
        session
            .show_menu(shown(hopper("other"), owner()))
            .await
            .unwrap();
        nothing(&mut client).await;
        assert!(closes(&mut session).is_empty());
        show(&mut session, &mut client, hopper("other"), Owner::Script).await;
        assert_eq!(
            closes(&mut session),
            [("other".into(), CloseReason::Replaced)]
        );
        container += 3;

        // The backend opens a screen of its own.
        let mut open = Writer::new();
        open.varint(5).varint(2).bytes(&[8, 0, 0]);
        let open = Packet::new(id(protocol, CB, "open_screen"), open.into_inner());
        send(&mut backend, &open).await;
        session.forward().await.unwrap();
        assert_eq!(next(&mut client).await, open);
        assert_eq!(
            closes(&mut session),
            [("other".into(), CloseReason::Server)]
        );
        // Opening a menu over it closes it on the backend, as Escape would.
        show(&mut session, &mut client, shop(), owner()).await;
        container += 1;
        flush(&mut session).await;
        assert_eq!(
            next(&mut backend).await,
            packets::client_close(v, 5).unwrap()
        );

        // Its container_close, a respawn and a new login end it too.
        for name in ["container_close", "respawn", "login"] {
            let packet = Packet::new(id(protocol, CB, name), vec![0]);
            send(&mut backend, &packet).await;
            session.forward().await.unwrap();
            assert_eq!(next(&mut client).await, packet);
            assert_eq!(
                closes(&mut session),
                [("shop".into(), CloseReason::Server)],
                "{name}"
            );
            session.joined = true;
            show(&mut session, &mut client, shop(), owner()).await;
            container += 1;
        }

        // A sign leaves the menu beneath it on the client: Rift closes it
        // first. So does a backend dialog.
        for packet in [
            Packet::new(id(protocol, CB, "open_sign_editor"), vec![0; 9]),
            Packet::new(0x8a, vec![1]),
        ] {
            send(&mut backend, &packet).await;
            session.forward().await.unwrap();
            assert_eq!(
                next(&mut client).await,
                packets::close(v, container).unwrap()
            );
            assert_eq!(next(&mut client).await, packet);
            assert_eq!(closes(&mut session), [("shop".into(), CloseReason::Server)]);
            show(&mut session, &mut client, shop(), owner()).await;
            container += 1;
        }

        // The player leaves.
        session.end_menus();
        assert_eq!(
            closes(&mut session),
            [("shop".into(), CloseReason::Disconnect)]
        );
        session.end_menus();
        assert!(closes(&mut session).is_empty());

        // A reconfiguration starts.
        show(&mut session, &mut client, shop(), owner()).await;
        send(&mut backend, &Packet::empty(0x74)).await;
        session.forward().await.unwrap();
        assert_eq!(closes(&mut session), [("shop".into(), CloseReason::Switch)]);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn menus_and_dialogs_replace_each_other() {
    timeout(Duration::from_secs(5), async {
        let protocol = 774;
        let (mut session, mut client, _backend) = menu_session(protocol).await;
        let dialog = Arc::new(
            crate::dialog::Dialog::parse(&json!({
                "type": "notice",
                "title": "Rules",
                "action": {"label": "OK", "action": {"type": "custom", "id": "ok"}},
            }))
            .unwrap(),
        );
        show(&mut session, &mut client, shop(), owner()).await;
        session.show_dialog(dialog.clone(), owner()).await.unwrap();
        // The menu is closed first, so it cannot come back with the dialog.
        assert_eq!(
            next(&mut client).await,
            packets::close(version(protocol), FIRST).unwrap()
        );
        assert_eq!(next(&mut client).await.id, 0x8a);
        assert_eq!(
            closes(&mut session),
            [("shop".into(), CloseReason::Replaced)]
        );

        // A menu replaces the dialog: its clicks are no longer Rift's.
        show(&mut session, &mut client, shop(), owner()).await;
        let id = dialog.wire()["action"]["action"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        send(&mut client, &custom_click(&id, None)).await;
        assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
        assert!(session.menu_open());
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_player_with_32_undelivered_events_gets_no_more_clicks_reported() {
    timeout(Duration::from_secs(5), async {
        let protocol = 774;
        let (mut session, mut client, _backend) = menu_session(protocol).await;
        show(&mut session, &mut client, shop(), owner()).await;
        let mut held = Vec::new();
        for _ in 0..crate::menu::MAX_PENDING_EVENTS {
            send(&mut client, &click(protocol, BUY, 0, 0, &[BUY])).await;
            held.push(delivery(session.forward().await.unwrap()));
        }
        // Still undone, but not reported.
        send(&mut client, &click(protocol, BUY, 0, 0, &[BUY])).await;
        assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
        flush(&mut session).await;
        assert_eq!(drain(&mut client).await.len(), 2 * 33);
        // Delivered events make room again.
        held.truncate(31);
        send(&mut client, &click(protocol, BUY, 0, 0, &[BUY])).await;
        delivery(session.forward().await.unwrap());
        // An ending is always delivered.
        session.end_menus();
        assert_eq!(closes(&mut session).len(), 1);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_server_switch_closes_the_menu_before_it_starts() {
    timeout(Duration::from_secs(5), async {
        let (mut session, mut client, mut backend) = session().await;
        play(&mut session, &mut client, &mut backend, Codec::default()).await;
        show(&mut session, &mut client, shop(), owner()).await;
        let (replacement, mut second) = tokio::io::duplex(65536);
        let switch = session.connect_backend(replacement);
        let peers = async {
            receive(&mut second, Codec::default()).await;
            assert_eq!(receive(&mut second, Codec::default()).await, login_start());
            Codec::default()
                .write(&mut second, &login_success())
                .await
                .unwrap();
            assert_eq!(
                receive(&mut second, Codec::default()).await,
                Packet::empty(3)
            );
            assert_eq!(
                next(&mut client).await,
                packets::close(version(774), FIRST).unwrap()
            );
            assert_eq!(next(&mut client).await, Packet::empty(0x74));
            Codec::default()
                .write(&mut client, &Packet::empty(0x0f))
                .await
                .unwrap();
            assert_eq!(next(&mut client).await, Packet::new(8, vec![0]));
        };
        let (result, ()) = tokio::join!(switch, peers);
        result.unwrap();
        assert_eq!(closes(&mut session), [("shop".into(), CloseReason::Switch)]);
        assert!(!session.menu_open());
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_translated_backend_s_rejection_is_caught_before_the_translator() {
    timeout(Duration::from_secs(5), async {
        let backend_protocol = 340;
        let (server, mut peer) = tokio::io::duplex(65536);
        let mut connection = Connection::new(server, State::Play);
        connection.translation = Some(Translation {
            translator: Translator::new(774, backend_protocol).unwrap().unwrap(),
            version: version(backend_protocol),
            forwarding: None,
            translated: VecDeque::new(),
        });
        assert_eq!(connection.protocol(version(774)), backend_protocol);
        // The request goes out in the backend's protocol, untranslated.
        let request = packets::inventory_request(backend_protocol).unwrap();
        connection.inject_raw(&request).unwrap();
        poll_fn(|cx| connection.poll_replies(cx)).await.unwrap();
        assert_eq!(next(&mut peer).await, request);
        // The rejection never reaches the translator; the acknowledgement
        // goes back at once.
        connection.awaiting_rejection = Some(backend_protocol);
        let rejection = Packet::new(
            id(backend_protocol, CB, "container_ack"),
            vec![0, 0xff, 0xff, 0],
        );
        send(&mut peer, &rejection).await;
        assert!(
            timeout(Duration::from_millis(50), connection.read_fresh())
                .await
                .is_err()
        );
        assert_eq!(
            next(&mut peer).await,
            packets::acknowledgement(backend_protocol).unwrap()
        );
        assert_eq!(connection.awaiting_rejection, None);
        assert!(
            connection
                .translation
                .as_ref()
                .unwrap()
                .translated
                .is_empty()
        );
    })
    .await
    .unwrap();
}

/// (slot, state ID) of each `container_set_slot` for container `container`.
fn slots_and_states(protocol: i32, container: i32, packets: &[Packet]) -> Vec<(i16, i32)> {
    packets
        .iter()
        .map(|packet| {
            assert_eq!(
                packet.id,
                id(protocol, CB, "container_set_slot"),
                "{packets:?}"
            );
            let mut r = WireReader::new(&packet.data);
            let slot = window::SetSlot::read(protocol, &mut r).unwrap();
            assert_eq!(slot.container, container);
            (slot.slot, slot.state_id)
        })
        .collect()
}

/// A click that names the state ID `state`, on the first container.
fn click_seen(protocol: i32, slot: i16, state: i32) -> Packet {
    let mut packet = click(protocol, slot, 0, 0, &[slot]);
    let mut r = WireReader::new(&packet.data);
    let mut click = window::Click::read(protocol, &mut r).unwrap();
    click.state_id = state;
    let mut w = Writer::new();
    click.write(protocol, &mut w).unwrap();
    packet.data = w.into_inner();
    packet
}

/// The shop with `change` applied to its definition.
fn shop_with(change: impl FnOnce(&mut serde_json::Value)) -> serde_json::Value {
    let mut spec = shop();
    change(&mut spec);
    spec
}

fn update(id: &str, patch: serde_json::Value, owner: Owner) -> crate::menu::Update {
    crate::menu::Update {
        owner,
        id: id.into(),
        patch: Arc::new(crate::menu::Patch::parse(&patch).unwrap()),
        heads: Arc::default(),
    }
}

#[tokio::test]
async fn a_redraw_sends_only_the_slots_that_changed() {
    timeout(Duration::from_secs(5), async {
        let protocol = 774;
        let (mut session, mut client, mut backend) = menu_session(protocol).await;
        show(&mut session, &mut client, shop(), owner()).await;
        let redrawn = shop_with(|spec| {
            spec["keys"]["B"]["name"] = json!("Cheap diamond");
            spec["keys"]["A"]["item"] = json!("golden_apple");
        });
        session
            .show_menu(shown(redrawn.clone(), owner()))
            .await
            .unwrap();
        let packets = drain(&mut client).await;
        assert_eq!(
            slots_and_states(protocol, FIRST, &packets),
            [(BUY, 2), (15, 2)]
        );
        // Each redraw names a new state.
        let again = shop_with(|spec| {
            spec["keys"]["B"]["name"] = json!("Cheap diamond");
            spec["keys"]["A"]["item"] = json!("golden_apple");
            spec["keys"]["D"]["lore"] = json!(["Dig it"]);
        });
        session
            .show_menu(shown(again.clone(), owner()))
            .await
            .unwrap();
        let packets = drain(&mut client).await;
        assert_eq!(slots_and_states(protocol, FIRST, &packets), [(DIG, 3)]);
        // The same definition sends nothing; a redraw is no ending.
        session.show_menu(shown(again, owner())).await.unwrap();
        nothing(&mut client).await;
        assert!(closes(&mut session).is_empty());
        assert!(session.menu_open());
        flush(&mut session).await;
        nothing(&mut backend).await;
        // Clicks still go to the redrawn menu.
        send(&mut client, &click_seen(protocol, BUY, 3)).await;
        let event = delivery(session.forward().await.unwrap()).event;
        assert_eq!(event.action.as_deref(), Some("buy"));
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_new_title_opens_the_same_container_again_with_every_slot() {
    timeout(Duration::from_secs(5), async {
        for protocol in [340, 774] {
            let (mut session, mut client, _backend) = menu_session(protocol).await;
            show(&mut session, &mut client, shop(), owner()).await;
            let titled = shop_with(|spec| spec["title"] = json!("Shop, page 2"));
            session
                .show_menu(shown(titled.clone(), owner()))
                .await
                .unwrap();
            let packets = drain(&mut client).await;
            let menu = Menu::parse(&titled).unwrap();
            assert_eq!(
                packets[0],
                packets::open_screen(version(protocol), FIRST, menu.kind(), menu.title()).unwrap()
            );
            let slots = slots_and_states(protocol, FIRST, &packets[1..]);
            assert_eq!(slots.len(), SHOP_FILLED, "{protocol}");
            assert!(slots.contains(&(BUY, if protocol >= 756 { 2 } else { 0 })));
            assert!(closes(&mut session).is_empty());
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn another_type_or_number_of_rows_opens_the_menu_anew_without_ending_it() {
    timeout(Duration::from_secs(5), async {
        let protocol = 774;
        let (mut session, mut client, mut backend) = menu_session(protocol).await;
        show(&mut session, &mut client, shop(), owner()).await;
        let taller = shop_with(|spec| {
            spec["layout"] = json!(["#########", "# B D A #", "#       #", "####X####"]);
        });
        let packets = show(&mut session, &mut client, taller.clone(), owner()).await;
        let menu = Menu::parse(&taller).unwrap();
        assert_eq!(
            packets[0],
            packets::open_screen(version(protocol), FIRST + 1, menu.kind(), menu.title()).unwrap()
        );
        assert_eq!(packets.len(), 1 + SHOP_FILLED + 2);
        let mut hopper = hopper("shop");
        hopper["title"] = json!("Shop");
        let packets = show(&mut session, &mut client, hopper, owner()).await;
        assert_eq!(slot_of(protocol, &packets[1]).0, FIRST + 2);
        assert!(closes(&mut session).is_empty());
        // A late click on the first container is still Rift's.
        send(&mut client, &click(protocol, BUY, 0, 0, &[BUY])).await;
        assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
        flush(&mut session).await;
        nothing(&mut backend).await;
        // The menu reopened by the same plugin ends once.
        session.end_menus();
        assert_eq!(
            closes(&mut session),
            [("shop".into(), CloseReason::Disconnect)]
        );
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_redraw_after_the_player_closed_the_menu_opens_it_anew() {
    timeout(Duration::from_secs(5), async {
        let protocol = 774;
        let (mut session, mut client, _backend) = menu_session(protocol).await;
        show(&mut session, &mut client, shop(), owner()).await;
        send(
            &mut client,
            &packets::client_close(version(protocol), FIRST).unwrap(),
        )
        .await;
        session.forward().await.unwrap();
        assert_eq!(closes(&mut session), [("shop".into(), CloseReason::Player)]);
        let packets = show(&mut session, &mut client, shop(), owner()).await;
        assert_eq!(packets.len(), 1 + SHOP_FILLED);
        assert_eq!(slot_of(protocol, &packets[1]).0, FIRST + 1);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn update_menu_changes_each_part_of_the_open_menu() {
    timeout(Duration::from_secs(5), async {
        let protocol = 774;
        let (mut session, mut client, mut backend) = menu_session(protocol).await;
        show(&mut session, &mut client, shop(), owner()).await;

        // Slots, one placed and one emptied.
        let patch = json!({"slots": [
            {"slot": 10, "item": "emerald", "id": "gem"},
            {"slot": 11, "empty": true},
        ]});
        session
            .update_menu(update("shop", patch, owner()))
            .await
            .unwrap();
        let packets = drain(&mut client).await;
        assert_eq!(
            slots_and_states(protocol, FIRST, &packets),
            [(10, 2), (BUY, 2)]
        );
        assert!(slot_of(protocol, &packets[0]).2);
        assert!(!slot_of(protocol, &packets[1]).2);
        send(&mut client, &click_seen(protocol, 10, 2)).await;
        let event = delivery(session.forward().await.unwrap()).event;
        assert_eq!(event.action.as_deref(), Some("gem"));
        flush(&mut session).await;
        drain(&mut client).await;

        // Keys: every slot the layout gave that character.
        let patch = json!({"keys": {"#": {"item": "black_stained_glass_pane", "name": " "}}});
        session
            .update_menu(update("shop", patch, owner()))
            .await
            .unwrap();
        let packets = drain(&mut client).await;
        assert_eq!(packets.len(), 19);
        assert!(
            slots_and_states(protocol, FIRST, &packets)
                .iter()
                .all(|(slot, _)| ![10, BUY, DIG, 15, CLOSE].contains(slot))
        );

        // State: nothing to send, and every later event carries it.
        let patch = json!({"state": {"page": 3}});
        session
            .update_menu(update("shop", patch, owner()))
            .await
            .unwrap();
        nothing(&mut client).await;
        send(&mut client, &click_seen(protocol, DIG, 4)).await;
        let event = delivery(session.forward().await.unwrap()).event;
        assert_eq!(event.state, Some(json!({"page": 3})));
        flush(&mut session).await;
        drain(&mut client).await;

        // Title: the same container again, with every slot.
        let patch = json!({"title": "Sold out"});
        session
            .update_menu(update("shop", patch, owner()))
            .await
            .unwrap();
        let packets = drain(&mut client).await;
        assert_eq!(packets[0].id, id(protocol, CB, "open_screen"));
        assert_eq!(
            slots_and_states(protocol, FIRST, &packets[1..]).len(),
            SHOP_FILLED
        );

        // Text is for anvils, and a slot outside the menu changes nothing.
        for patch in [
            json!({"text": "x"}),
            json!({"slots": [{"slot": 27, "item": "stone"}]}),
        ] {
            session
                .update_menu(update("shop", patch, owner()))
                .await
                .unwrap();
            nothing(&mut client).await;
        }

        // Another plugin's menu, or another id, is not changed.
        let patch = json!({"title": "Mine"});
        session
            .update_menu(update("shop", patch.clone(), Owner::Script))
            .await
            .unwrap();
        session
            .update_menu(update("other", patch.clone(), owner()))
            .await
            .unwrap();
        nothing(&mut client).await;

        // A menu that is gone is not changed either.
        session.close_menu(&owner(), None).await.unwrap();
        drain(&mut client).await;
        session
            .update_menu(update("shop", patch, owner()))
            .await
            .unwrap();
        nothing(&mut client).await;
        assert!(!session.menu_open());
        let ends = session.take_menu_events();
        assert_eq!(ends.len(), 1);
        assert_eq!(ends[0].event.state, Some(json!({"page": 3})));
        flush(&mut session).await;
        nothing(&mut backend).await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_click_on_an_action_the_player_did_not_see_is_undone_and_not_reported() {
    timeout(Duration::from_secs(5), async {
        let protocol = 774;
        let (mut session, mut client, mut backend) = menu_session(protocol).await;
        show(&mut session, &mut client, shop(), owner()).await;
        // At state 2: the buy button costs more, the dig button only looks
        // different, and the close button gets an id.
        let redrawn = shop_with(|spec| {
            spec["keys"]["B"]["payload"] = json!({"price": 6});
            spec["keys"]["D"]["name"] = json!("Dig");
            spec["keys"]["X"]["id"] = json!("leave");
        });
        session.show_menu(shown(redrawn, owner())).await.unwrap();
        let packets = drain(&mut client).await;
        assert_eq!(
            slots_and_states(protocol, FIRST, &packets),
            [(BUY, 2), (DIG, 2), (CLOSE, 2)]
        );

        // Clicks made on what state 1 showed.
        send(&mut client, &click_seen(protocol, BUY, 1)).await;
        assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
        flush(&mut session).await;
        assert_undone(protocol, &mut client, FIRST, &[BUY]).await;
        send(&mut client, &click_seen(protocol, DIG, 1)).await;
        let event = delivery(session.forward().await.unwrap()).event;
        assert_eq!(event.action.as_deref(), Some("dig"));
        send(&mut client, &click_seen(protocol, 15, 1)).await;
        let event = delivery(session.forward().await.unwrap()).event;
        assert_eq!(event.action.as_deref(), Some("eat"));
        send(&mut client, &click_seen(protocol, CLOSE, 1)).await;
        assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
        assert!(session.menu_open());
        flush(&mut session).await;
        drain(&mut client).await;

        // The same clicks on what state 2 showed.
        send(&mut client, &click_seen(protocol, BUY, 2)).await;
        let event = delivery(session.forward().await.unwrap()).event;
        assert_eq!(event.payload, Some(json!({"price": 6})));
        send(&mut client, &click_seen(protocol, CLOSE, 2)).await;
        let event = delivery(session.forward().await.unwrap()).event;
        assert_eq!(event.action.as_deref(), Some("leave"));
        assert!(!session.menu_open());
        assert_eq!(closes(&mut session), [("shop".into(), CloseReason::Plugin)]);
        flush(&mut session).await;
        nothing(&mut backend).await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn clients_before_1_17_1_report_every_click() {
    timeout(Duration::from_secs(5), async {
        for protocol in [340, 755] {
            let (mut session, mut client, _backend) = menu_session(protocol).await;
            show(&mut session, &mut client, shop(), owner()).await;
            let redrawn = shop_with(|spec| spec["keys"]["B"]["payload"] = json!({"price": 6}));
            session.show_menu(shown(redrawn, owner())).await.unwrap();
            let packets = drain(&mut client).await;
            assert_eq!(slots_and_states(protocol, FIRST, &packets), [(BUY, 0)]);
            send(&mut client, &click(protocol, BUY, 0, 0, &[BUY])).await;
            let event = delivery(session.forward().await.unwrap()).event;
            assert_eq!(event.payload, Some(json!({"price": 6})), "{protocol}");
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_shift_click_on_the_inventory_rows_before_1_17_puts_every_menu_slot_back() {
    timeout(Duration::from_secs(5), async {
        let protocol = 340;
        let (mut session, mut client, _backend) = menu_session(protocol).await;
        show(&mut session, &mut client, shop(), owner()).await;
        send(&mut client, &click(protocol, 40, 0, 1, &[])).await;
        assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
        flush(&mut session).await;
        let all: Vec<i16> = (0..27).collect();
        assert_undone(protocol, &mut client, FIRST, &all).await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn an_unanswered_inventory_request_counts_as_lost_after_a_while() {
    timeout(Duration::from_secs(5), async {
        for protocol in [340, 767, 774] {
            let (mut session, mut client, mut backend) = menu_session(protocol).await;
            show(&mut session, &mut client, shop(), owner()).await;
            let request = packets::inventory_request(protocol).unwrap();
            let shift = click(protocol, BUY, 0, 1, &[BUY, 40]);
            send(&mut client, &shift).await;
            session.forward().await.unwrap();
            flush(&mut session).await;
            assert_eq!(next(&mut backend).await, request);
            // The backend never answers: the next click waits ...
            send(&mut client, &shift).await;
            session.forward().await.unwrap();
            flush(&mut session).await;
            nothing(&mut backend).await;
            // ... until the request counts as lost.
            let lost = std::time::Instant::now()
                .checked_sub(crate::session::menus::REQUEST_TIMEOUT)
                .unwrap();
            session.menus.request.sent = Some(lost);
            send(&mut client, &shift).await;
            session.forward().await.unwrap();
            flush(&mut session).await;
            assert_eq!(next(&mut backend).await, request, "{protocol}");
            // A request waiting for the menu to close is not held back by a
            // lost one either.
            session.menus.request.sent = Some(lost);
            if protocol < 768 {
                send(&mut backend, &hotbar_slot(protocol)).await;
                session.forward().await.unwrap();
                session.close_menu(&owner(), None).await.unwrap();
                flush(&mut session).await;
                assert_eq!(next(&mut backend).await, request, "{protocol}");
            }
            drain(&mut client).await;
            nothing(&mut backend).await;
        }
    })
    .await
    .unwrap();
}
