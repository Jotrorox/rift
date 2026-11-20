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
    session.in_world = true;
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
            assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
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
            // The answer came: the click that waited asks once more, at the
            // end of the interval between requests.
            assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
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
async fn creative_slots_are_dropped_while_a_menu_is_open_and_just_after_rift_closed_it() {
    timeout(Duration::from_secs(5), async {
        let protocol = 774;
        let (mut session, mut client, mut backend) = menu_session(protocol).await;
        let creative = Packet::new(id(protocol, SB, "set_creative_mode_slot"), vec![0, 36, 0]);
        show(&mut session, &mut client, shop(), owner()).await;
        send(&mut client, &creative).await;
        assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
        nothing(&mut backend).await;
        // Rift closed the menu: one the client sent before it knew is
        // still dropped.
        session.close_menu(&owner(), None).await.unwrap();
        send(&mut client, &creative).await;
        assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
        nothing(&mut backend).await;
        // A second later the player's own creative actions pass again.
        session.menus.rift_ended = Some(std::time::Instant::now() - menus::CREATIVE_GRACE);
        send(&mut client, &creative).await;
        session.forward().await.unwrap();
        assert_eq!(next(&mut backend).await, creative);
        // The player closed it themselves: no grace is needed.
        drain(&mut client).await;
        show(&mut session, &mut client, shop(), owner()).await;
        let close = packets::client_close(version(protocol), FIRST + 1).unwrap();
        send(&mut client, &close).await;
        assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
        assert!(!session.menu_open());
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
async fn each_outstanding_request_takes_one_rejection_out_and_no_more() {
    timeout(Duration::from_secs(5), async {
        let protocol = 340;
        let (server, mut peer) = tokio::io::duplex(65536);
        let mut connection = Connection::new(server, State::Play);
        let rejection = Packet::new(id(protocol, CB, "container_ack"), vec![0, 0xff, 0xff, 0]);
        let acknowledgement = packets::acknowledgement(protocol).unwrap();
        // Two requests, the second after the first counted as lost: both
        // rejections are Rift's.
        connection.expect_rejection(protocol);
        connection.expect_rejection(protocol);
        for _ in 0..2 {
            send(&mut peer, &rejection).await;
            assert!(
                timeout(Duration::from_millis(50), connection.read_fresh())
                    .await
                    .is_err()
            );
            assert_eq!(next(&mut peer).await, acknowledgement);
        }
        // None is outstanding: Rift no longer looks, and a rejection that is
        // not Rift's passes.
        assert_eq!(connection.awaiting_rejection, None);
        send(&mut peer, &rejection).await;
        assert_eq!(
            connection.read_fresh().await.unwrap(),
            Some(rejection.clone())
        );
        // A backend that never rejects: Rift stops looking long after the
        // request counted as lost.
        connection.expect_rejection(protocol);
        if let Some(rejections) = connection.awaiting_rejection.as_mut() {
            rejections.last = std::time::Instant::now() - REJECTION_WAIT;
        }
        let other = Packet::new(id(protocol, CB, "set_time"), vec![0; 16]);
        send(&mut peer, &other).await;
        assert_eq!(connection.read_fresh().await.unwrap(), Some(other));
        assert_eq!(connection.awaiting_rejection, None);
        send(&mut peer, &rejection).await;
        assert_eq!(connection.read_fresh().await.unwrap(), Some(rejection));
        nothing(&mut peer).await;
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
        connection.expect_rejection(backend_protocol);
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
            session.menus.request.last = Some(lost);
            send(&mut client, &shift).await;
            session.forward().await.unwrap();
            flush(&mut session).await;
            assert_eq!(next(&mut backend).await, request, "{protocol}");
            // A request waiting for the menu to close is not held back by a
            // lost one either.
            session.menus.request.sent = Some(lost);
            session.menus.request.last = Some(lost);
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

#[tokio::test]
async fn a_player_in_a_world_rift_cannot_switch_from_gets_menus_and_dialogs() {
    timeout(Duration::from_secs(5), async {
        let protocol = 774;
        let (mut session, mut client, mut backend) = menu_session(protocol).await;
        session.joined = false;
        session.in_world = false;
        // Between servers the menu is not shown, and its plugin hears so.
        session.show_menu(shown(shop(), owner())).await.unwrap();
        nothing(&mut client).await;
        assert!(!session.menu_open());
        assert_eq!(closes(&mut session), [("shop".into(), CloseReason::Switch)]);
        // A Join Game Rift could not switch away from, such as a world that
        // enforces secure chat for an offline session (here one Rift cannot
        // read at all), still puts the player in a world.
        let login = Packet::new(id(protocol, CB, "login"), vec![0]);
        send(&mut backend, &login).await;
        session.forward().await.unwrap();
        assert_eq!(next(&mut client).await, login);
        assert!(!session.joined && !session.can_switch());
        show(&mut session, &mut client, shop(), owner()).await;
        assert!(session.menu_open());
        let dialog = Arc::new(
            crate::dialog::Dialog::parse(&json!({"type": "notice", "title": "Rules"})).unwrap(),
        );
        session.show_dialog(dialog, owner()).await.unwrap();
        let packets = drain(&mut client).await;
        assert_eq!(packets.last().unwrap().id, 0x8a);
        assert_eq!(
            closes(&mut session),
            [("shop".into(), CloseReason::Replaced)]
        );
        // A reconfiguration takes the player out of the world again.
        send(&mut backend, &Packet::empty(0x74)).await;
        session.forward().await.unwrap();
        session.show_menu(shown(shop(), owner())).await.unwrap();
        assert_eq!(closes(&mut session), [("shop".into(), CloseReason::Switch)]);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn inventory_requests_keep_an_interval_and_the_last_one_always_goes_out() {
    timeout(Duration::from_secs(5), async {
        for protocol in [340, 774] {
            let (mut session, mut client, mut backend) = menu_session(protocol).await;
            show(&mut session, &mut client, shop(), owner()).await;
            let request = packets::inventory_request(protocol).unwrap();
            let shift = click(protocol, BUY, 0, 1, &[BUY, 40]);
            let started = std::time::Instant::now();
            send(&mut client, &shift).await;
            session.forward().await.unwrap();
            flush(&mut session).await;
            assert_eq!(next(&mut backend).await, request);
            // Answered at once, and three more shift-clicks right away: no
            // request until the interval is over, then one.
            if protocol < 756 {
                let rejection =
                    Packet::new(id(protocol, CB, "container_ack"), vec![0, 0xff, 0xff, 0]);
                send(&mut backend, &rejection).await;
            }
            send(&mut backend, &inventory(protocol)).await;
            session.forward().await.unwrap();
            for _ in 0..3 {
                send(&mut client, &shift).await;
                session.forward().await.unwrap();
            }
            flush(&mut session).await;
            if protocol < 756 {
                assert_eq!(
                    next(&mut backend).await,
                    packets::acknowledgement(protocol).unwrap()
                );
            }
            nothing(&mut backend).await;
            assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
            flush(&mut session).await;
            assert_eq!(next(&mut backend).await, request, "{protocol}");
            assert!(started.elapsed() >= crate::session::menus::REQUEST_INTERVAL);
            nothing(&mut backend).await;
            drain(&mut client).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_request_that_waited_for_a_lost_one_goes_out_when_it_counts_as_lost() {
    timeout(Duration::from_secs(5), async {
        let protocol = 774;
        let (mut session, mut client, mut backend) = menu_session(protocol).await;
        show(&mut session, &mut client, shop(), owner()).await;
        let request = packets::inventory_request(protocol).unwrap();
        let shift = click(protocol, BUY, 0, 1, &[BUY, 40]);
        send(&mut client, &shift).await;
        session.forward().await.unwrap();
        flush(&mut session).await;
        assert_eq!(next(&mut backend).await, request);
        send(&mut client, &shift).await;
        session.forward().await.unwrap();
        flush(&mut session).await;
        nothing(&mut backend).await;
        // The backend never answers. Without another click, the waiting
        // request goes out once the first counts as lost.
        let almost = std::time::Instant::now()
            .checked_sub(crate::session::menus::REQUEST_TIMEOUT - Duration::from_millis(100))
            .unwrap();
        session.menus.request.sent = Some(almost);
        session.menus.request.last = Some(almost);
        assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
        flush(&mut session).await;
        assert_eq!(next(&mut backend).await, request);
        nothing(&mut backend).await;
        drain(&mut client).await;
    })
    .await
    .unwrap();
}

fn nick() -> serde_json::Value {
    json!({"id": "nick", "type": "anvil", "title": "Choose a nickname", "text": "Steve"})
}

const LEFT: i16 = crate::menu::ANVIL_LEFT as i16;
const RIGHT: i16 = crate::menu::ANVIL_RIGHT as i16;
const RESULT: i16 = crate::menu::ANVIL_RESULT as i16;

/// What a client on `protocol` sends while the player types `text`:
/// `rename_item` from 1.13, the `MC|ItemName` plugin message before.
fn rename(protocol: i32, text: &str) -> Packet {
    let mut w = Writer::new();
    if protocol >= 393 {
        w.string(text);
        Packet::new(id(protocol, SB, "rename_item"), w.into_inner())
    } else {
        w.string("MC|ItemName").string(text);
        Packet::new(id(protocol, SB, "custom_payload"), w.into_inner())
    }
}

/// (slot, stack bytes) of a `container_set_slot`.
fn slot_stack(protocol: i32, packet: &Packet) -> (i16, Vec<u8>) {
    assert_eq!(
        packet.id,
        id(protocol, CB, "container_set_slot"),
        "{packet:?}"
    );
    let mut r = WireReader::new(&packet.data);
    if protocol >= 768 {
        r.varint().unwrap();
    } else {
        r.i8().unwrap();
    }
    if protocol >= 756 {
        r.varint().unwrap();
    }
    let slot = r.i16().unwrap();
    (slot, r.rest().to_vec())
}

/// Slot `slot` of `menu` as the client should get it.
fn expected(menu: &Menu, slot: i16, protocol: i32) -> (i16, Vec<u8>) {
    let stack = menu
        .slot_stack(
            slot as usize,
            protocol,
            &|_| None,
            crate::menu::StackCache::shared(),
        )
        .unwrap();
    (slot, stack.to_vec())
}

/// The nick prompt after the player typed `text`; `field` when Rift put
/// the text in the field since.
fn typed(text: &str, field: bool) -> Menu {
    let mut menu = Menu::parse(&nick()).unwrap();
    menu.typed(text.into());
    if field {
        menu.field_from_text();
    }
    menu
}

fn experience(protocol: i32) -> Packet {
    Packet::new(id(protocol, CB, "set_experience"), vec![0, 0, 0, 0, 5, 7])
}

#[tokio::test]
async fn an_anvil_opens_with_its_slots_and_a_cost_of_0() {
    timeout(Duration::from_secs(5), async {
        for protocol in [340, 754, 777] {
            let v = version(protocol);
            let (mut session, mut client, mut backend) = menu_session(protocol).await;
            let packets = show(&mut session, &mut client, nick(), owner()).await;
            let menu = Menu::parse(&nick()).unwrap();
            assert_eq!(
                packets[0],
                packets::open_screen(v, FIRST, menu.kind(), menu.title()).unwrap()
            );
            // The right input stays empty; the result is the default.
            assert_eq!(packets.len(), 4, "{protocol}");
            assert_eq!(
                slot_stack(protocol, &packets[1]),
                expected(&menu, LEFT, protocol)
            );
            assert_eq!(
                slot_stack(protocol, &packets[2]),
                expected(&menu, RESULT, protocol)
            );
            assert_eq!(packets[3], packets::anvil_cost(v, FIRST).unwrap());
            assert!(session.menu_open());

            // Custom buttons: a name tag on the left, a cancel button as
            // the result.
            let custom = json!({
                "id": "pick", "type": "anvil", "text": "x", "layout": ["P X"],
                "keys": {
                    "P": {"item": "name_tag"},
                    "X": {"item": "barrier", "id": "cancel", "close": true},
                },
            });
            let packets = show(&mut session, &mut client, custom.clone(), owner()).await;
            let menu = Menu::parse(&custom).unwrap();
            assert_eq!(packets.len(), 4);
            assert_eq!(
                slot_stack(protocol, &packets[1]),
                expected(&menu, LEFT, protocol)
            );
            assert_eq!(
                slot_stack(protocol, &packets[2]),
                expected(&menu, RESULT, protocol)
            );
            assert_eq!(packets[3], packets::anvil_cost(v, FIRST + 1).unwrap());
            assert_eq!(
                closes(&mut session),
                [("nick".into(), CloseReason::Replaced)]
            );
            nothing(&mut backend).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn typing_renames_the_result_and_reaches_nobody_but_rift() {
    timeout(Duration::from_secs(5), async {
        for protocol in [340, 754, 777] {
            let v = version(protocol);
            let (mut session, mut client, mut backend) = menu_session(protocol).await;
            show(&mut session, &mut client, nick(), owner()).await;
            send(&mut client, &rename(protocol, "Alex")).await;
            assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
            flush(&mut session).await;
            // The result again, named after the text, and the cost; never
            // the left item, which would reset the field.
            let packets = drain(&mut client).await;
            assert_eq!(packets.len(), 2, "{protocol} {packets:?}");
            assert_eq!(
                slot_stack(protocol, &packets[0]),
                expected(&typed("Alex", false), RESULT, protocol)
            );
            assert_eq!(packets[1], packets::anvil_cost(v, FIRST).unwrap());
            // The same text again changes nothing, but the client computed
            // its own result for it: that is put back.
            send(&mut client, &rename(protocol, "Alex")).await;
            assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
            flush(&mut session).await;
            let again = drain(&mut client).await;
            assert_eq!(again.len(), 2);
            assert_eq!(
                slot_stack(protocol, &again[0]),
                slot_stack(protocol, &packets[0])
            );
            // Longer than the client allows is ignored, as the game does.
            let limit = crate::menu::anvil_text_limit(protocol);
            send(&mut client, &rename(protocol, &"x".repeat(limit + 1))).await;
            assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
            flush(&mut session).await;
            nothing(&mut client).await;
            nothing(&mut backend).await;

            // The backend's experience bar passes on and is remembered.
            send(&mut backend, &experience(protocol)).await;
            session.forward().await.unwrap();
            assert_eq!(next(&mut client).await, experience(protocol));

            // "OK": reported with the text, then the three slots (the left
            // named after the text now), the cost, the experience bar and
            // the cursor.
            send(&mut client, &click(protocol, RESULT, 0, 0, &[RESULT])).await;
            let event = delivery(session.forward().await.unwrap()).event;
            assert_eq!(
                (event.kind, event.action.as_deref(), event.text.as_deref()),
                (EventKind::Click, Some("submit"), Some("Alex"))
            );
            flush(&mut session).await;
            let packets = drain(&mut client).await;
            assert_eq!(packets.len(), 6, "{protocol} {packets:?}");
            let menu = typed("Alex", true);
            assert_eq!(
                slot_stack(protocol, &packets[0]),
                expected(&menu, LEFT, protocol)
            );
            assert_eq!(
                slot_stack(protocol, &packets[1]),
                (RIGHT, crate::menu::stack::encode(protocol, None).unwrap())
            );
            assert_eq!(
                slot_stack(protocol, &packets[2]),
                expected(&menu, RESULT, protocol)
            );
            assert_eq!(packets[3], packets::anvil_cost(v, FIRST).unwrap());
            assert_eq!(packets[4], experience(protocol));
            assert_empty_cursor(protocol, &packets[5]);
            nothing(&mut backend).await;

            // A malformed rename closes the session.
            let mut broken = rename(protocol, "x");
            broken.data.push(0);
            send(&mut client, &broken).await;
            assert!(session.forward().await.is_err());
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn typing_events_come_at_most_5_a_second_and_always_for_the_final_text() {
    timeout(Duration::from_secs(5), async {
        let protocol = 777;
        let (mut session, mut client, _backend) = menu_session(protocol).await;
        let mut spec = nick();
        spec["typing"] = json!(true);
        spec["state"] = json!({"step": 1});
        show(&mut session, &mut client, spec, owner()).await;
        let started = std::time::Instant::now();
        send(&mut client, &rename(protocol, "a")).await;
        let event = delivery(session.forward().await.unwrap()).event;
        assert_eq!(
            (event.kind, event.text.as_deref(), event.state),
            (EventKind::Text, Some("a"), Some(json!({"step": 1})))
        );
        for text in ["ab", "abc"] {
            send(&mut client, &rename(protocol, text)).await;
            assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
        }
        // An unchanged text is no event.
        send(&mut client, &rename(protocol, "abc")).await;
        assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
        // The last text comes once the interval is over.
        let event = delivery(session.forward().await.unwrap()).event;
        assert_eq!(
            (event.kind, event.text.as_deref()),
            (EventKind::Text, Some("abc"))
        );
        assert!(started.elapsed() >= Duration::from_millis(200));
        assert!(
            timeout(Duration::from_millis(300), session.forward())
                .await
                .is_err(),
            "no event after the last text"
        );
        // Later typing is reported at once again.
        send(&mut client, &rename(protocol, "abcd")).await;
        let event = delivery(session.forward().await.unwrap()).event;
        assert_eq!(event.text.as_deref(), Some("abcd"));
        // A text that waits when the menu closes is in its close event.
        send(&mut client, &rename(protocol, "abcde")).await;
        assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
        session.close_menu(&owner(), None).await.unwrap();
        let ends = session.take_menu_events();
        assert_eq!(ends.len(), 1);
        assert_eq!(ends[0].event.text.as_deref(), Some("abcde"));
        assert!(
            timeout(Duration::from_millis(300), session.forward())
                .await
                .is_err()
        );
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_redraw_keeps_the_text_and_a_patch_replaces_it() {
    timeout(Duration::from_secs(5), async {
        for protocol in [340, 754, 777] {
            let v = version(protocol);
            let (mut session, mut client, _backend) = menu_session(protocol).await;
            show(&mut session, &mut client, nick(), owner()).await;
            send(&mut client, &rename(protocol, "Alex")).await;
            session.forward().await.unwrap();
            flush(&mut session).await;
            drain(&mut client).await;

            // The same definition again sends nothing.
            session.show_menu(shown(nick(), owner())).await.unwrap();
            nothing(&mut client).await;

            // A new title: the screen again with the same container, every
            // slot with the left item named after what the player typed,
            // and the cost.
            let mut titled = nick();
            titled["title"] = json!("Letters only, 3 to 16");
            session.show_menu(shown(titled, owner())).await.unwrap();
            let packets = drain(&mut client).await;
            assert_eq!(packets[0].id, id(protocol, CB, "open_screen"));
            assert_eq!(packets.len(), 4, "{protocol}");
            let menu = typed("Alex", true);
            assert_eq!(
                slot_stack(protocol, &packets[1]),
                expected(&menu, LEFT, protocol)
            );
            assert_eq!(
                slot_stack(protocol, &packets[2]),
                expected(&menu, RESULT, protocol)
            );
            assert_eq!(packets[3], packets::anvil_cost(v, FIRST).unwrap());

            // A patch with text puts it in the field and the result.
            session
                .update_menu(update("nick", json!({"text": "Bob"}), owner()))
                .await
                .unwrap();
            let packets = drain(&mut client).await;
            let mut menu = Menu::parse(&nick()).unwrap();
            menu.apply(&crate::menu::Patch::parse(&json!({"text": "Bob"})).unwrap())
                .unwrap();
            assert_eq!(packets.len(), 3, "{protocol} {packets:?}");
            assert_eq!(
                slot_stack(protocol, &packets[0]),
                expected(&menu, LEFT, protocol)
            );
            assert_eq!(
                slot_stack(protocol, &packets[1]),
                expected(&menu, RESULT, protocol)
            );
            assert_eq!(packets[2], packets::anvil_cost(v, FIRST).unwrap());

            // The player types again, and the plugin puts back the text the
            // left item already carries: it goes through an empty slot, or
            // the client would keep what the player typed.
            send(&mut client, &rename(protocol, "Bobby")).await;
            session.forward().await.unwrap();
            flush(&mut session).await;
            drain(&mut client).await;
            session
                .update_menu(update("nick", json!({"text": "Bob"}), owner()))
                .await
                .unwrap();
            let packets = drain(&mut client).await;
            assert_eq!(packets.len(), 4, "{protocol} {packets:?}");
            assert_eq!(
                slot_stack(protocol, &packets[0]),
                (LEFT, crate::menu::stack::encode(protocol, None).unwrap())
            );
            assert_eq!(
                slot_stack(protocol, &packets[1]),
                expected(&menu, LEFT, protocol)
            );
            assert_eq!(
                slot_stack(protocol, &packets[2]),
                expected(&menu, RESULT, protocol)
            );

            // Events carry it.
            send(&mut client, &click(protocol, RESULT, 0, 0, &[RESULT])).await;
            let event = delivery(session.forward().await.unwrap()).event;
            assert_eq!(event.text.as_deref(), Some("Bob"));
            assert!(closes(&mut session).is_empty());
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn clicks_on_an_anvil_put_every_input_back_and_shift_clicks_too() {
    timeout(Duration::from_secs(5), async {
        for protocol in [340, 754, 777] {
            let v = version(protocol);
            let (mut session, mut client, mut backend) = menu_session(protocol).await;
            show(&mut session, &mut client, nick(), owner()).await;
            // A shift-click on an inventory item moves it into the inputs on
            // the client.
            let changed: &[i16] = if protocol >= 755 { &[RIGHT, 10] } else { &[] };
            send(&mut client, &click(protocol, 10, 0, 1, changed)).await;
            assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
            flush(&mut session).await;
            let packets = drain(&mut client).await;
            assert_eq!(packets.len(), 5, "{protocol} {packets:?}");
            let slots: Vec<i16> = packets[..3]
                .iter()
                .map(|packet| slot_stack(protocol, packet).0)
                .collect();
            assert_eq!(slots, [LEFT, RIGHT, RESULT]);
            assert_eq!(packets[3], packets::anvil_cost(v, FIRST).unwrap());
            assert_empty_cursor(protocol, &packets[4]);
            assert_eq!(
                next(&mut backend).await,
                packets::inventory_request(protocol).unwrap()
            );
            // Taking the paper out of the left slot.
            send(&mut client, &click(protocol, LEFT, 0, 0, &[LEFT])).await;
            assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
            flush(&mut session).await;
            assert_eq!(drain(&mut client).await.len(), 5);
        }
    })
    .await
    .unwrap();
}

/// Show the nick prompt and type `text` into it.
async fn open_typed(
    session: &mut Session<DuplexStream, DuplexStream>,
    client: &mut DuplexStream,
    text: &str,
) {
    let protocol = session.handshake.protocol;
    show(session, client, nick(), owner()).await;
    send(client, &rename(protocol, text)).await;
    session.forward().await.unwrap();
    flush(session).await;
    drain(client).await;
}

#[tokio::test]
async fn an_anvil_ends_once_with_its_text_in_every_way() {
    timeout(Duration::from_secs(5), async {
        let protocol = 774;
        let v = version(protocol);
        let (mut session, mut client, mut backend) = menu_session(protocol).await;
        let mut container = FIRST;
        let ends = |session: &mut Session<DuplexStream, DuplexStream>| {
            session
                .take_menu_events()
                .into_iter()
                .map(|delivery| {
                    (
                        delivery.event.reason.unwrap(),
                        delivery.event.text.unwrap_or_default(),
                    )
                })
                .collect::<Vec<_>>()
        };
        open_typed(&mut session, &mut client, "esc").await;
        send(&mut client, &packets::client_close(v, container).unwrap()).await;
        session.forward().await.unwrap();
        assert_eq!(ends(&mut session), [(CloseReason::Player, "esc".into())]);

        open_typed(&mut session, &mut client, "plugin").await;
        container += 1;
        session.close_menu(&owner(), Some("nick")).await.unwrap();
        assert_eq!(
            next(&mut client).await,
            packets::close(v, container).unwrap()
        );
        assert_eq!(ends(&mut session), [(CloseReason::Plugin, "plugin".into())]);

        open_typed(&mut session, &mut client, "replaced").await;
        show(&mut session, &mut client, shop(), owner()).await;
        assert_eq!(
            ends(&mut session),
            [(CloseReason::Replaced, "replaced".into())]
        );

        open_typed(&mut session, &mut client, "server").await;
        // The chest it replaced carries no text.
        assert_eq!(ends(&mut session), [(CloseReason::Replaced, String::new())]);
        let close = Packet::new(id(protocol, CB, "container_close"), vec![0]);
        send(&mut backend, &close).await;
        session.forward().await.unwrap();
        assert_eq!(next(&mut client).await, close);
        assert_eq!(ends(&mut session), [(CloseReason::Server, "server".into())]);

        open_typed(&mut session, &mut client, "gone").await;
        session.end_menus();
        assert_eq!(
            ends(&mut session),
            [(CloseReason::Disconnect, "gone".into())]
        );
        session.end_menus();
        assert!(ends(&mut session).is_empty());
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn renames_are_rift_s_only_while_its_anvil_was_the_last_screen() {
    timeout(Duration::from_secs(5), async {
        let protocol = 774;
        let v = version(protocol);
        let (mut session, mut client, mut backend) = menu_session(protocol).await;
        // No Rift anvil: the backend's.
        send(&mut client, &rename(protocol, "x")).await;
        session.forward().await.unwrap();
        assert_eq!(next(&mut backend).await, rename(protocol, "x"));
        // A late one after Rift's anvil closed is dropped.
        show(&mut session, &mut client, nick(), owner()).await;
        send(&mut client, &packets::client_close(v, FIRST).unwrap()).await;
        session.forward().await.unwrap();
        send(&mut client, &rename(protocol, "late")).await;
        assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
        flush(&mut session).await;
        nothing(&mut backend).await;
        nothing(&mut client).await;
        // Once the backend opens a screen of its own, they are its again.
        let mut open = Writer::new();
        open.varint(5).varint(7).bytes(&[8, 0, 0]);
        let open = Packet::new(id(protocol, CB, "open_screen"), open.into_inner());
        send(&mut backend, &open).await;
        session.forward().await.unwrap();
        assert_eq!(next(&mut client).await, open);
        send(&mut client, &rename(protocol, "theirs")).await;
        session.forward().await.unwrap();
        assert_eq!(next(&mut backend).await, rename(protocol, "theirs"));
        // A chest of Rift's is no anvil either.
        show(&mut session, &mut client, shop(), owner()).await;
        flush(&mut session).await;
        drain(&mut backend).await;
        send(&mut client, &rename(protocol, "chest")).await;
        session.forward().await.unwrap();
        assert_eq!(next(&mut backend).await, rename(protocol, "chest"));
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_client_that_does_not_read_cannot_pile_up_undone_clicks() {
    timeout(Duration::from_secs(10), async {
        let protocol = 774;
        let (mut session, mut client, _backend) = menu_session(protocol).await;
        show(&mut session, &mut client, shop(), owner()).await;
        // Drags over every menu slot, each undone with 27 slots, from a
        // client that never reads what Rift sends it.
        let all: Vec<i16> = (0..27).collect();
        let drag = click(protocol, -999, 1, 5, &all);
        let mut blocked = false;
        for _ in 0..5000 {
            send(&mut client, &drag).await;
            match timeout(Duration::from_millis(100), session.forward()).await {
                Ok(event) => assert_eq!(event.unwrap(), SessionEvent::Packet),
                Err(_) => {
                    blocked = true;
                    break;
                }
            }
        }
        assert!(blocked, "Rift kept reading a client that does not read");
        assert!(
            session.client.replies.len() <= 2 * MAX_QUEUED_REPLIES,
            "{} bytes queued",
            session.client.replies.len()
        );
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn late_clicks_ask_a_1_17_backend_no_more_often_than_the_interval() {
    timeout(Duration::from_secs(5), async {
        let protocol = 755;
        let (mut session, mut client, mut backend) = menu_session(protocol).await;
        show(&mut session, &mut client, shop(), owner()).await;
        send(
            &mut client,
            &packets::client_close(version(protocol), FIRST).unwrap(),
        )
        .await;
        session.forward().await.unwrap();
        // Late shift-clicks on the closed menu, as fast as a client sends
        // them: one request, and one more at the end of the interval.
        let request = packets::inventory_request(protocol).unwrap();
        let shift = click(protocol, BUY, 0, 1, &[BUY, 40]);
        for _ in 0..20 {
            send(&mut client, &shift).await;
            assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
        }
        flush(&mut session).await;
        assert_eq!(next(&mut backend).await, request);
        nothing(&mut backend).await;
        assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
        flush(&mut session).await;
        assert_eq!(next(&mut backend).await, request);
        nothing(&mut backend).await;
        // A menu that opens meanwhile turns the waiting request into one
        // when it closes, and the timer does not spin.
        send(&mut client, &shift).await;
        session.forward().await.unwrap();
        show(&mut session, &mut client, shop(), owner()).await;
        assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
        assert_eq!(session.menus.deadline(), None);
        assert!(session.menus.request.on_close);
        flush(&mut session).await;
        nothing(&mut backend).await;
    })
    .await
    .unwrap();
}
