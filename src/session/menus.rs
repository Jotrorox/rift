//! Inventory menus in a session (docs/menus.md, "How the proxy does it").
//!
//! Rift writes a menu's packets to the client itself, in the client's
//! protocol, and takes the client's packets for its container out of the
//! stream before the backend or the translator sees them. A click is undone
//! on the client, because nothing really moved, and reported to whoever
//! showed the menu. When a click disturbed the player's own inventory rows,
//! the backend is asked to send the inventory again, in its own protocol and
//! past the translator.
//!
//! The only state kept for a session without a menu is the backend's open
//! container and whether an inventory request is on its way.
use super::{Connection, Session, SessionEvent};
use crate::{
    dialog::Owner,
    menu::{self, CloseReason, Delivery, Menu, MenuEvent, Shown, StackCache, Ticket},
    protocol::{
        self, Packet, ProtocolVersion, State,
        menu::{self as packets, BackendPacket, ClientPacket, Ids, Recipe},
    },
};
use std::{
    collections::VecDeque,
    future::poll_fn,
    io,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::io::{AsyncRead, AsyncWrite};

/// Clients up to 1.21.1 apply a `container_set_slot` for the player's
/// inventory only to the hotbar while another container is open.
const WHOLE_INVENTORY_SLOTS: i32 = 768;
/// State IDs count up to this and start again at 1.
const MAX_STATE_ID: i32 = 0x7fff;

/// The menu a player has open.
pub(crate) struct OpenMenu {
    /// What the client shows, resolved in the player's language.
    pub(crate) menu: Menu,
    pub(crate) owner: Owner,
    /// Rift's container ID for this menu, one of 101 to 127.
    pub(crate) container: i32,
    /// The state ID of Rift's last update of the container, for 1.17.1 and
    /// newer clients.
    pub(crate) state: i32,
    /// Each slot's stack as the client reads it.
    pub(crate) stacks: Vec<Arc<[u8]>>,
    /// The skins of the players its heads name, for slots built again.
    pub(crate) shown: Shown,
}

impl OpenMenu {
    fn slots(&self) -> usize {
        self.menu.kind().slots()
    }

    /// Each slot's stack for a client on `protocol`, with the skins the menu
    /// came with.
    fn build_stacks(&mut self, protocol: i32) -> io::Result<()> {
        let shown = &self.shown;
        self.stacks = (0..self.slots())
            .map(|slot| {
                self.menu.slot_stack(
                    slot,
                    protocol,
                    &|player| shown.head(player),
                    StackCache::shared(),
                )
            })
            .collect::<io::Result<_>>()?;
        Ok(())
    }

    /// The state ID of the next update.
    fn next_state(&mut self) -> i32 {
        self.state = if self.state >= MAX_STATE_ID {
            1
        } else {
            self.state + 1
        };
        self.state
    }
}

/// Whether the backend is being asked for the player's inventory.
#[derive(Debug, Default)]
struct Request {
    /// A request is on its way: until its answer, after the rejection's
    /// acknowledgement where there is one.
    in_flight: bool,
    /// Another click disturbed the inventory rows meanwhile.
    again: bool,
    /// Ask when the menu closes: a 1.17 backend, or a single-slot update the
    /// client dropped while the menu was open.
    on_close: bool,
}

#[derive(Default)]
pub(crate) struct Menus {
    /// The client protocol's packet IDs, once looked up.
    ids: Option<Option<Ids>>,
    pub(crate) open: Option<OpenMenu>,
    /// The container ID the next menu takes.
    next_container: i32,
    /// Rift's previous container and its number of slots: late packets for
    /// it are still Rift's, until the backend opens that ID itself.
    previous: Option<(i32, usize)>,
    /// The container the backend has open, from its `open_screen` and the
    /// close packets of both sides.
    backend_container: Option<i32>,
    request: Request,
    /// Ends of menus, waiting for the session's owner to deliver them.
    closed: VecDeque<Delivery>,
    /// Events handed out and not yet delivered to their plugin.
    undelivered: Arc<AtomicUsize>,
}

impl Menus {
    fn ids(&mut self, version: ProtocolVersion) -> Option<Ids> {
        *self.ids.get_or_insert_with(|| Ids::of(version.number()))
    }

    /// A fresh container ID, never the backend's.
    fn take_container(&mut self) -> i32 {
        let next = |id: i32| {
            if id >= packets::LAST_CONTAINER {
                packets::FIRST_CONTAINER
            } else {
                id + 1
            }
        };
        let mut id = self
            .next_container
            .clamp(packets::FIRST_CONTAINER, packets::LAST_CONTAINER);
        if Some(id) == self.backend_container {
            id = next(id);
        }
        self.next_container = next(id);
        id
    }

    fn delivery(&self, owner: Owner, mut event: MenuEvent) -> Delivery {
        event.ticket = Ticket::new(&self.undelivered);
        Delivery { owner, event }
    }

    /// End the open menu. `client` tells the client to close it, for an
    /// ending it does not know of; `backend` is asked for the inventory if a
    /// request waits for the menu to close.
    fn end<C, B>(
        &mut self,
        version: ProtocolVersion,
        reason: CloseReason,
        client: Option<&mut Connection<C>>,
        backend: Option<&mut Connection<B>>,
    ) -> io::Result<()> {
        let Some(open) = self.open.take() else {
            return Ok(());
        };
        self.previous = Some((open.container, open.slots()));
        if let Some(client) = client {
            client.inject(&packets::close(version, open.container)?)?;
        }
        let event = MenuEvent::close(&open.menu, reason);
        let delivery = self.delivery(open.owner, event);
        self.closed.push_back(delivery);
        let leaving = matches!(reason, CloseReason::Switch | CloseReason::Disconnect);
        if !leaving
            && std::mem::take(&mut self.request.on_close)
            && let Some(backend) = backend
        {
            self.request_inventory(version, backend)?;
        }
        Ok(())
    }

    /// Ask the backend to send the player's inventory again, by the recipe
    /// of its protocol, with at most one request on its way.
    fn request_inventory<B>(
        &mut self,
        version: ProtocolVersion,
        backend: &mut Connection<B>,
    ) -> io::Result<()> {
        if !backend.state.settled(State::Play) {
            return Ok(());
        }
        let protocol = backend.protocol(version);
        let recipe = packets::recipe(protocol);
        match recipe {
            // Its answer comes slot by slot, which the client drops while a
            // menu is open.
            Recipe::ClaimAll if self.open.is_some() => {
                self.request.on_close = true;
                return Ok(());
            }
            Recipe::ClaimAll => {}
            Recipe::StateId | Recipe::Transaction => {
                if self.request.in_flight {
                    self.request.again = true;
                    return Ok(());
                }
                self.request.in_flight = true;
            }
        }
        if recipe == Recipe::Transaction {
            backend.awaiting_rejection = Some(protocol);
        }
        backend.inject_raw(&packets::inventory_request(protocol)?)
    }

    /// Follow a clientbound backend packet in play, in the client's
    /// protocol, before it is passed on.
    pub(super) fn backend_packet<C, B>(
        &mut self,
        version: ProtocolVersion,
        packet: &Packet,
        client: &mut Connection<C>,
        backend: &mut Connection<B>,
    ) -> io::Result<()> {
        let Some(ids) = self.ids(version) else {
            return Ok(());
        };
        let dialog = protocol::dialog::backend(version, State::Play, packet)
            == Some(protocol::dialog::BackendDialog::Shown);
        match packets::backend_packet(&ids, packet) {
            Some(BackendPacket::Opened(container)) => {
                // The backend's screen replaces Rift's on the client.
                self.end::<C, B>(version, CloseReason::Server, None, Some(backend))?;
                self.backend_container = Some(container);
                if self
                    .previous
                    .is_some_and(|(previous, _)| previous == container)
                {
                    self.previous = None;
                }
            }
            Some(BackendPacket::Closed | BackendPacket::World) => {
                self.end::<C, B>(version, CloseReason::Server, None, Some(backend))?;
                self.backend_container = None;
            }
            // A sign or a book leaves the menu open beneath it on the
            // client: Rift closes it first.
            Some(BackendPacket::Screen) => {
                self.end(version, CloseReason::Server, Some(client), Some(backend))?;
            }
            Some(BackendPacket::InventorySlot) => {
                if self.open.is_some() && version.number() < WHOLE_INVENTORY_SLOTS {
                    self.request.on_close = true;
                }
            }
            Some(BackendPacket::InventoryContent) => {
                self.request.in_flight = false;
                if std::mem::take(&mut self.request.again) {
                    self.request_inventory(version, backend)?;
                }
            }
            None if dialog => {
                self.end(version, CloseReason::Server, Some(client), Some(backend))?;
            }
            None => {}
        }
        Ok(())
    }

    /// The backend is about to change: a switch or a reconfiguration. The
    /// open menu ends, and what Rift knew of the backend's screens and
    /// requests goes with it.
    fn leave_backend<C, B>(&mut self, version: ProtocolVersion) -> io::Result<()> {
        self.end::<C, B>(version, CloseReason::Switch, None, None)?;
        self.backend_container = None;
        self.request = Request::default();
        Ok(())
    }
}

impl<C: AsyncRead + AsyncWrite + Unpin, B: AsyncRead + AsyncWrite + Unpin> Session<C, B> {
    /// Show a menu (1.12+), replacing any menu or dialog Rift has open on the
    /// client; showing the open menu's `id` again for the same owner opens it
    /// anew without ending it. Its events go to `shown.owner` as
    /// [`SessionEvent::Menu`] and [`Self::take_menu_events`]. A player who is
    /// not in a world (between servers) does not get it.
    pub async fn show_menu(&mut self, shown: Shown) -> io::Result<()> {
        self.client.flush_pending().await?;
        if !self.joined || !self.client.state.settled(State::Play) {
            return Ok(());
        }
        let version = self.version()?;
        let protocol = version.number();
        if !packets::supported(protocol) || shown.menu.kind() == menu::Kind::Anvil {
            return Ok(());
        }
        let mut open = OpenMenu {
            menu: Arc::unwrap_or_clone(shown.menu.clone()),
            owner: shown.owner.clone(),
            container: 0,
            state: 0,
            stacks: Vec::new(),
            shown,
        };
        if let Err(error) = open.build_stacks(protocol) {
            log_error!("menu {}: {error}", open.menu.id());
            return Ok(());
        }
        let container = self.menus.take_container();
        open.container = container;
        let state = open.next_state();
        let mut sent = vec![packets::open_screen(
            version,
            container,
            open.menu.kind(),
            open.menu.title(),
        )?];
        for (slot, button) in open.menu.slots().iter().enumerate() {
            if button.is_some() {
                sent.push(packets::set_slot(
                    version,
                    container,
                    state,
                    slot as i16,
                    &open.stacks[slot],
                )?);
            }
        }
        if let Some(old) = self.menus.open.take() {
            self.menus.previous = Some((old.container, old.slots()));
            // Showing the open menu again is a redraw, not an ending.
            if old.menu.id() != open.menu.id() || old.owner != open.owner {
                let event = MenuEvent::close(&old.menu, CloseReason::Replaced);
                let delivery = self.menus.delivery(old.owner, event);
                self.menus.closed.push_back(delivery);
            }
        } else if let Some(backend_container) = self.menus.backend_container.take()
            && let Some(backend) = self.backend.as_mut()
        {
            // The client replaces the backend's screen without telling it.
            backend.inject(&packets::client_close(version, backend_container)?)?;
        }
        // The menu's screen replaces a dialog's.
        self.dialog = None;
        self.menus.open = Some(open);
        for packet in &sent {
            self.client.inject(packet)?;
        }
        self.flush_injected().await
    }

    /// Close the menu Rift shows: any menu (`id` is `None`), or only the
    /// menu `id` of `owner`. Its owner gets the `plugin` ending.
    pub async fn close_menu(&mut self, owner: &Owner, id: Option<&str>) -> io::Result<()> {
        let matches = self.menus.open.as_ref().is_some_and(|open| match id {
            None => true,
            Some(id) => open.menu.id() == id && open.owner == *owner,
        });
        if !matches {
            return Ok(());
        }
        self.client.flush_pending().await?;
        self.end_menu(CloseReason::Plugin, true)?;
        self.flush_injected().await
    }

    /// The menu events the session holds for its owner to deliver, oldest
    /// first: every ending of a menu. Clicks come as [`SessionEvent::Menu`].
    pub fn take_menu_events(&mut self) -> Vec<Delivery> {
        self.menus.closed.drain(..).collect()
    }

    /// The player left: the open menu ends with `disconnect`.
    pub fn end_menus(&mut self) {
        if let Ok(version) = self.version() {
            let _ = self
                .menus
                .end::<C, B>(version, CloseReason::Disconnect, None, None);
        }
    }

    /// Whether a menu of Rift's is open on the client.
    pub fn menu_open(&self) -> bool {
        self.menus.open.is_some()
    }

    /// End the open menu with `reason`; `tell_client` closes it on the
    /// client.
    pub(super) fn end_menu(&mut self, reason: CloseReason, tell_client: bool) -> io::Result<()> {
        let version = self.version()?;
        self.menus.end(
            version,
            reason,
            tell_client.then_some(&mut self.client),
            self.backend.as_mut(),
        )
    }

    /// The switch to another backend starts: Rift closes the menu.
    pub(super) async fn end_menu_for_switch(&mut self) -> io::Result<()> {
        let version = self.version()?;
        let open = self.menus.open.is_some();
        if open {
            self.client.flush_pending().await?;
            self.end_menu(CloseReason::Switch, true)?;
        }
        self.menus.leave_backend::<C, B>(version)?;
        self.flush_injected().await
    }

    /// The backend starts a reconfiguration, which closes every screen.
    pub(super) fn end_menu_for_configuration(&mut self) -> io::Result<()> {
        let version = self.version()?;
        self.menus.leave_backend::<C, B>(version)
    }

    /// Write what Rift queued for the client now. Cancel-safe: what is not
    /// written stays queued for the next forward.
    async fn flush_injected(&mut self) -> io::Result<()> {
        poll_fn(|cx| self.client.poll_replies(cx)).await
    }

    /// A serverbound packet in play: Rift's when it names Rift's container,
    /// or is a creative slot while a menu is open. `None` passes it on.
    pub(super) fn menu_client_packet(
        &mut self,
        version: ProtocolVersion,
        packet: &Packet,
    ) -> io::Result<Option<SessionEvent>> {
        let Some(ids) = self.menus.ids(version) else {
            return Ok(None);
        };
        // A client in creative mode would create the item it drops.
        if self.menus.open.is_some() && packets::creative(&ids, packet) {
            return Ok(Some(SessionEvent::Packet));
        }
        // A packet Rift cannot read is not Rift's: the backend judges it.
        let Some((kind, container)) = packets::client_container(&ids, packet).ok().flatten() else {
            return Ok(None);
        };
        if self
            .menus
            .open
            .as_ref()
            .is_some_and(|open| open.container == container)
        {
            return Ok(Some(match kind {
                ClientPacket::Click => self.menu_click(version, &ids, packet)?,
                ClientPacket::Close => {
                    self.end_menu(CloseReason::Player, false)?;
                    SessionEvent::Packet
                }
                ClientPacket::Other => SessionEvent::Packet,
            }));
        }
        if let Some((previous, slots)) = self.menus.previous
            && previous == container
        {
            // A late packet for a menu that is gone. The client may still
            // have moved its own items.
            if kind == ClientPacket::Click
                && packets::Click::read(&ids, packet)?.touches_inventory(slots)
                && let Some(backend) = self.backend.as_mut()
            {
                self.menus.request_inventory(version, backend)?;
            }
            return Ok(Some(SessionEvent::Packet));
        }
        if kind == ClientPacket::Close && self.menus.backend_container == Some(container) {
            self.menus.backend_container = None;
        }
        Ok(None)
    }

    /// A click on the open menu: undo what the client predicted, ask for the
    /// inventory if its rows were touched, and report the click if the
    /// button reports this kind.
    fn menu_click(
        &mut self,
        version: ProtocolVersion,
        ids: &Ids,
        packet: &Packet,
    ) -> io::Result<SessionEvent> {
        let click = packets::Click::read(ids, packet)?;
        let undelivered = self.menus.undelivered.load(Ordering::Relaxed);
        let Some(open) = self.menus.open.as_mut() else {
            return Ok(SessionEvent::Packet);
        };
        let slots = open.slots();
        let state = open.next_state();
        for slot in click.menu_slots(slots) {
            let stack = packets::set_slot(
                version,
                open.container,
                state,
                slot as i16,
                &open.stacks[slot],
            )?;
            self.client.inject(&stack)?;
        }
        self.client
            .inject(&packets::empty_cursor(version, state)?)?;
        let mut report = None;
        let mut close = false;
        if click.on_menu(slots)
            && let Some((kind, key)) = click.kind()
            && let Some(button) = open.menu.button(i32::from(click.slot))
        {
            let slot = click.slot as usize;
            if button.action.reports(kind) && undelivered < menu::MAX_PENDING_EVENTS {
                report = Some((
                    open.owner.clone(),
                    MenuEvent::click(&open.menu, slot, kind, key),
                ));
            }
            close = button.action.closes(kind);
        }
        if click.touches_inventory(slots)
            && let Some(backend) = self.backend.as_mut()
        {
            self.menus.request_inventory(version, backend)?;
        }
        let event = match report {
            Some((owner, event)) => SessionEvent::Menu(self.menus.delivery(owner, event)),
            None => SessionEvent::Packet,
        };
        if close {
            self.end_menu(CloseReason::Plugin, true)?;
        }
        Ok(event)
    }
}
