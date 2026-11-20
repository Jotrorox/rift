//! Inventory menus in a session (docs/menus-internals.md).
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
//! container, the containers Rift used, and whether an inventory request is
//! on its way.
use super::{Connection, Session, SessionEvent};
use crate::{
    dialog::Owner,
    menu::{
        self, ANVIL_LEFT, ANVIL_RESULT, ANVIL_RIGHT, CloseReason, Delivery, Menu, MenuEvent, Shown,
        StackCache, Ticket, Update,
    },
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
    time::{Duration, Instant},
};
use tokio::io::{AsyncRead, AsyncWrite};

/// Clients up to 1.21.1 apply a `container_set_slot` for the player's
/// inventory only to the hotbar while another container is open.
const WHOLE_INVENTORY_SLOTS: i32 = 768;
/// State IDs count up to this and start again at 1; 0 is a menu that got no
/// slot yet.
const MAX_STATE_ID: i32 = 0x7fff;
/// An inventory request without an answer after this long counts as lost:
/// one that waited for it goes out then.
pub(super) const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Rift asks a backend for the inventory at most this often. A need in
/// between is met at the end of the interval, so the last disturbance is
/// always repaired.
pub(super) const REQUEST_INTERVAL: Duration = Duration::from_millis(250);
/// The shortest time between two `text` events of an anvil.
const TEXT_EVENT_INTERVAL: Duration =
    Duration::from_millis(1000 / menu::TEXT_EVENTS_PER_SECOND as u64);
/// How long after Rift ended a menu on its own (not the player) a
/// `set_creative_mode_slot` is still dropped. The client learns of the end
/// one round trip later and may meanwhile have sent one for a button, which
/// the backend would create as a real item. A creative player who really
/// drops or takes an item in that second loses that one action.
pub(super) const CREATIVE_GRACE: Duration = Duration::from_secs(1);
/// An anvil's result is built again for a new text at most this often. The
/// client sends a rename on every key, and building the result converts its
/// stack down to the client's version (about 40 steps on 1.12), so a client
/// that types or sends renames fast costs at most this rate. A text that has
/// to wait is built when the interval ends, so the final one always is;
/// meanwhile each rename gets the last result back. The plugin's `text` is
/// the latest at once either way.
pub(super) const RESULT_INTERVAL: Duration = Duration::from_millis(50);
/// Rift's container IDs, 101 to 127.
const CONTAINERS: usize = (packets::LAST_CONTAINER - packets::FIRST_CONTAINER + 1) as usize;

/// The state ID the client reads for Rift's `updates`-th update of a
/// container.
fn state_id(updates: u64) -> i32 {
    match updates {
        0 => 0,
        // The remainder is below MAX_STATE_ID.
        n => ((n - 1) % MAX_STATE_ID as u64) as i32 + 1,
    }
}

/// The update a client's state ID `state` names after Rift's `updates`
/// updates: the latest one with that ID. 0 when it names none, as for a
/// menu that got no slot since it opened or since its title changed.
fn seen(updates: u64, state: i32) -> u64 {
    if !(1..=MAX_STATE_ID).contains(&state) {
        return 0;
    }
    let age = (state_id(updates) - state).rem_euclid(MAX_STATE_ID);
    updates.saturating_sub(age as u64)
}

/// The menu a player has open.
pub(crate) struct OpenMenu {
    /// What the client shows, resolved in the player's language.
    pub(crate) menu: Menu,
    pub(crate) owner: Owner,
    /// Rift's container ID for this menu, one of 101 to 127.
    pub(crate) container: i32,
    /// Rift's updates of the container since it opened, for the state IDs of
    /// 1.17.1 and newer clients.
    updates: u64,
    /// For each slot, the update that first showed its current action
    /// (`id`, `payload` and `close`); 0 for the action it opened with.
    changed: Vec<u64>,
    /// Each slot's stack as the client reads it.
    pub(crate) stacks: Vec<Arc<[u8]>>,
    /// The skins of the players its heads name, for slots built again.
    pub(crate) shown: Shown,
    /// Anvil with `typing`: when the last `text` event was reported, and
    /// whether a newer text waits for the next one.
    last_text: Option<Instant>,
    text_pending: bool,
    /// Anvil: when the result was last built for a new text, and whether a
    /// newer text waits for it (see [`RESULT_INTERVAL`]).
    pub(super) result_built: Option<Instant>,
    pub(super) result_stale: bool,
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

    /// The player typed a new text: whether to report it now, within
    /// [`TEXT_EVENT_INTERVAL`] of the last report. One that has to wait is
    /// reported when the interval ends (see [`Self::text_due`]), so the
    /// final text always is.
    fn typed_now(&mut self, now: Instant) -> bool {
        if self
            .last_text
            .is_some_and(|last| now.duration_since(last) < TEXT_EVENT_INTERVAL)
        {
            self.text_pending = true;
            return false;
        }
        self.typed_reported(now);
        true
    }

    fn typed_reported(&mut self, now: Instant) {
        self.last_text = Some(now);
        self.text_pending = false;
    }

    /// When the text that waits is reported.
    fn text_due(&self) -> Option<Instant> {
        let last = self.last_text.filter(|_| self.text_pending)?;
        Some(last + TEXT_EVENT_INTERVAL)
    }

    /// The player typed a new text: build the result for it now, unless it
    /// was built within [`RESULT_INTERVAL`]; then it waits for
    /// [`Self::result_due`].
    fn typed_result(&mut self, now: Instant, protocol: i32) -> io::Result<()> {
        self.result_stale = true;
        if self
            .result_built
            .is_some_and(|built| now.duration_since(built) < RESULT_INTERVAL)
        {
            return Ok(());
        }
        self.fresh_result(now, protocol)
    }

    /// Build the result for the current text if a newer one waits.
    fn fresh_result(&mut self, now: Instant, protocol: i32) -> io::Result<()> {
        if std::mem::take(&mut self.result_stale) {
            self.result_built = Some(now);
            self.rebuild(ANVIL_RESULT, protocol)?;
        }
        Ok(())
    }

    /// When the result for the text that waits is built.
    fn result_due(&self) -> Option<Instant> {
        let built = self.result_built.filter(|_| self.result_stale)?;
        Some(built + RESULT_INTERVAL)
    }

    /// Build slot `slot`'s stack again after the menu changed.
    fn rebuild(&mut self, slot: usize, protocol: i32) -> io::Result<()> {
        let shown = &self.shown;
        let stack = self.menu.slot_stack(
            slot,
            protocol,
            &|player| shown.head(player),
            StackCache::shared(),
        )?;
        if let Some(old) = self.stacks.get_mut(slot) {
            *old = stack;
        }
        Ok(())
    }

    fn anvil(&self) -> bool {
        self.menu.kind() == menu::Kind::Anvil
    }

    /// The state ID of the next update.
    fn next_state(&mut self) -> i32 {
        self.updates += 1;
        state_id(self.updates)
    }

    /// Whether a click that names the state ID `state` was made on an older
    /// action than the slot has now. Clients before 1.17.1 name none, and
    /// their clicks are never stale.
    fn stale(&self, slot: usize, state: Option<i32>) -> bool {
        state.is_some_and(|state| {
            seen(self.updates, state) < self.changed.get(slot).copied().unwrap_or_default()
        })
    }
}

/// Whether the backend is being asked for the player's inventory.
#[derive(Debug, Default)]
pub(super) struct Request {
    /// When the request on its way was sent; it is on its way until its
    /// answer, after the rejection's acknowledgement where there is one, or
    /// until [`REQUEST_TIMEOUT`] passed.
    pub(super) sent: Option<Instant>,
    /// When the last request went out, for [`REQUEST_INTERVAL`].
    pub(super) last: Option<Instant>,
    /// Another click disturbed the inventory rows meanwhile: a request goes
    /// out when the one on its way is answered or lost, and the interval
    /// is over.
    pub(super) again: bool,
    /// Ask when the menu closes: a 1.17 backend, or a single-slot update the
    /// client dropped while the menu was open.
    pub(super) on_close: bool,
}

impl Request {
    fn in_flight(&self) -> bool {
        self.sent
            .is_some_and(|sent| sent.elapsed() < REQUEST_TIMEOUT)
    }

    /// When the request that waits goes out without another reason: when
    /// the one on its way counts as lost, or at the end of the interval.
    fn due(&self) -> Option<Instant> {
        if !self.again {
            return None;
        }
        Some(match self.sent {
            Some(sent) if self.in_flight() => sent + REQUEST_TIMEOUT,
            _ => self
                .last
                .map_or_else(Instant::now, |last| last + REQUEST_INTERVAL),
        })
    }
}

#[derive(Default)]
pub(crate) struct Menus {
    /// The client protocol's packet IDs, once looked up.
    ids: Option<Option<Ids>>,
    pub(crate) open: Option<OpenMenu>,
    /// The container ID the next menu takes.
    next_container: i32,
    /// For each of Rift's container IDs that Rift used, its number of slots;
    /// 0 for one it did not use or the backend opened since. Late packets
    /// for those are still Rift's.
    used: [u8; CONTAINERS],
    /// The container the backend has open, from its `open_screen` and the
    /// close packets of both sides.
    backend_container: Option<i32>,
    pub(super) request: Request,
    /// The backend's last `set_experience`, in the client's protocol, which
    /// a click on an anvil's result is followed by again.
    experience: Option<Packet>,
    /// The last menu Rift opened was an anvil, and the backend opened no
    /// container since: a late rename is still Rift's.
    late_anvil: bool,
    /// Ends of menus, waiting for the session's owner to deliver them.
    closed: VecDeque<Delivery>,
    /// When Rift last ended a menu for another reason than the player
    /// closing it, for [`CREATIVE_GRACE`].
    pub(super) rift_ended: Option<Instant>,
    /// Events handed out and not yet delivered to their plugin.
    undelivered: Arc<AtomicUsize>,
}

/// Who showed a menu, for logs.
fn describe(owner: &Owner) -> String {
    match owner {
        Owner::Script => "rift.lua".into(),
        Owner::Plugin(name) => format!("plugin {name}"),
        Owner::Remote {
            peer, plugin: None, ..
        } => format!("rift.lua of peer {peer}"),
        Owner::Remote {
            peer,
            plugin: Some(name),
            ..
        } => format!("plugin {name} of peer {peer}"),
    }
}

/// The warning for a patch that does not fit the open menu: which menu of
/// which plugin, and why.
fn patch_not_applied(update: &Update, error: &str) -> String {
    format!(
        "menu {} of {}: patch not applied: {error}",
        update.id,
        describe(&update.owner)
    )
}

/// The index of Rift's container `id` in [`Menus::used`].
fn used_index(id: i32) -> Option<usize> {
    usize::try_from(id - packets::FIRST_CONTAINER)
        .ok()
        .filter(|index| *index < CONTAINERS)
}

impl Menus {
    fn ids(&mut self, version: ProtocolVersion) -> Option<Ids> {
        *self.ids.get_or_insert_with(|| Ids::of(version.number()))
    }

    /// A fresh container ID for a menu of `slots` slots, never the
    /// backend's.
    fn take_container(&mut self, slots: usize) -> i32 {
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
        if let Some(index) = used_index(id) {
            self.used[index] = u8::try_from(slots).unwrap_or(u8::MAX);
        }
        id
    }

    /// The number of slots of the menu Rift showed earlier with container
    /// `id`, if a late packet for it is still Rift's.
    fn earlier(&self, id: i32) -> Option<usize> {
        let slots = self.used[used_index(id)?];
        (slots > 0).then_some(usize::from(slots))
    }

    fn delivery(&self, owner: Owner, mut event: MenuEvent) -> Delivery {
        event.ticket = Ticket::new(&self.undelivered);
        Delivery { owner, event }
    }

    /// A menu that was never shown still ends with its one `close` event,
    /// so its plugin learns that it did not reach the player.
    fn refuse(&mut self, shown: &Shown, reason: CloseReason) {
        let event = MenuEvent::close(&shown.menu, reason);
        let delivery = self.delivery(shown.owner.clone(), event);
        self.closed.push_back(delivery);
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
        if let Some(client) = client {
            client.inject(&packets::close(version, open.container)?)?;
        }
        if reason != CloseReason::Player {
            self.rift_ended = Some(Instant::now());
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
            // The backend is changing; what it had is gone with it.
            self.request.again = false;
            return Ok(());
        }
        let protocol = backend.protocol(version);
        let recipe = packets::recipe(protocol);
        if recipe == Recipe::ClaimAll && self.open.is_some() {
            // Its answer comes slot by slot, which the client drops while a
            // menu is open. A request that waited goes out then too.
            self.request.on_close = true;
            self.request.again = false;
            return Ok(());
        }
        let now = Instant::now();
        let soon = self
            .request
            .last
            .is_some_and(|last| now.duration_since(last) < REQUEST_INTERVAL);
        // A 1.17 backend's answer is not followed: its requests only keep
        // the interval.
        let waits = recipe != Recipe::ClaimAll && self.request.in_flight();
        if waits || soon {
            self.request.again = true;
            return Ok(());
        }
        if recipe != Recipe::ClaimAll {
            self.request.sent = Some(now);
        }
        self.request.last = Some(now);
        self.request.again = false;
        if recipe == Recipe::Transaction {
            backend.expect_rejection(protocol);
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
                self.late_anvil = false;
                // Packets for that ID are the backend's from now on.
                if let Some(index) = used_index(container) {
                    self.used[index] = 0;
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
            Some(BackendPacket::Experience) => self.experience = Some(packet.clone()),
            Some(BackendPacket::InventoryContent) => {
                self.request.sent = None;
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

    /// When the session has to act on its own next: a waiting inventory
    /// request, or the last text the player typed in an anvil, to report or
    /// to build the result for.
    pub(super) fn deadline(&self) -> Option<Instant> {
        let open = self.open.as_ref();
        [
            self.request.due(),
            open.and_then(OpenMenu::text_due),
            open.and_then(OpenMenu::result_due),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// The backend is about to change: a switch or a reconfiguration. The
    /// open menu ends, and what Rift knew of the backend's screens and
    /// requests goes with it.
    fn leave_backend<C, B>(&mut self, version: ProtocolVersion) -> io::Result<()> {
        self.end::<C, B>(version, CloseReason::Switch, None, None)?;
        self.backend_container = None;
        self.request = Request::default();
        self.experience = None;
        Ok(())
    }
}

impl<C: AsyncRead + AsyncWrite + Unpin, B: AsyncRead + AsyncWrite + Unpin> Session<C, B> {
    /// Show a menu (1.12+), replacing any menu or dialog Rift has open on the
    /// client. Showing the `id` of the open menu again for the same owner
    /// redraws it: only what changed is sent, and it does not end. Its
    /// events go to `shown.owner` as [`SessionEvent::Menu`] and
    /// [`Self::take_menu_events`]. A player who is not in a world (between
    /// servers) does not get it, and its owner gets the `switch` ending.
    pub async fn show_menu(&mut self, shown: Shown) -> io::Result<()> {
        self.client.flush_pending().await?;
        if !self.in_world() {
            self.menu_undeliverable(&shown);
            return Ok(());
        }
        let version = self.version()?;
        let protocol = version.number();
        if !packets::supported(protocol) {
            log_warn!(
                "menu {} of {}: not shown: the client cannot show it",
                shown.menu.id(),
                describe(&shown.owner)
            );
            self.menus.refuse(&shown, CloseReason::Server);
            return Ok(());
        }
        let mut menu = Arc::unwrap_or_clone(shown.menu.clone());
        menu.fit_text(protocol);
        if let Some(open) = &self.menus.open
            && open.menu.id() == menu.id()
            && open.owner == shown.owner
        {
            menu.keep_text(&open.menu);
            if !open.menu.diff(&menu).reopen {
                self.redraw(version, menu, shown)?;
                return self.flush_injected().await;
            }
        }
        let mut open = OpenMenu {
            changed: vec![0; menu.kind().slots()],
            menu,
            owner: shown.owner.clone(),
            container: 0,
            updates: 0,
            stacks: Vec::new(),
            shown,
            last_text: None,
            text_pending: false,
            result_built: None,
            result_stale: false,
        };
        if let Err(error) = open.build_stacks(protocol) {
            log_warn!(
                "menu {} of {}: not shown: {error}",
                open.menu.id(),
                describe(&open.owner)
            );
            self.menus.refuse(&open.shown, CloseReason::Server);
            return Ok(());
        }
        let container = self.menus.take_container(open.slots());
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
        let anvil = open.anvil();
        if anvil {
            // After the client's own result and cost for the left item.
            sent.push(packets::anvil_cost(version, container)?);
        }
        self.menus.late_anvil = anvil;
        if let Some(old) = self.menus.open.take() {
            // The same menu with another type or number of rows opens anew,
            // which is no ending either.
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

    /// Change part of the menu `id` of `owner`, if the player still has it
    /// open (docs/menus.md, "Changing an open menu"). A patch that names a
    /// slot outside the menu changes nothing.
    pub async fn update_menu(&mut self, update: Update) -> io::Result<()> {
        let Some(open) = self
            .menus
            .open
            .as_ref()
            .filter(|open| open.menu.id() == update.id && open.owner == update.owner)
        else {
            return Ok(());
        };
        let mut menu = open.menu.clone();
        if let Err(error) = menu.apply(&update.patch) {
            // The plugin's call returned long ago: it could not know the
            // menu its patch would meet.
            log_warn!("{}", patch_not_applied(&update, &error));
            return Ok(());
        }
        let mut heads = (*open.shown.heads).clone();
        heads.extend(
            update
                .heads
                .iter()
                .map(|(player, textures)| (player.clone(), textures.clone())),
        );
        let shown = Shown {
            menu: Arc::new(menu.clone()),
            owner: update.owner,
            heads: Arc::new(heads),
        };
        self.client.flush_pending().await?;
        let version = self.version()?;
        self.redraw(version, menu, shown)?;
        self.flush_injected().await
    }

    /// Turn the open menu into `menu`, of the same type and rows, by
    /// sending the client only what changed: the slots that look different
    /// or do something else, or for a new title the screen again with the
    /// same container and every slot, because the client builds it anew.
    ///
    /// An anvil keeps what the player typed. Its client fills the text field
    /// again whenever the left item changes, so that item is named after the
    /// current text before it is sent; a patch with `text` puts the new text
    /// there through it. Whatever input is sent, the result and a cost of 0
    /// follow, because the client computes its own for the inputs.
    fn redraw(&mut self, version: ProtocolVersion, mut menu: Menu, shown: Shown) -> io::Result<()> {
        let Some(open) = self.menus.open.as_mut() else {
            return Ok(());
        };
        let protocol = version.number();
        let anvil = open.anvil();
        menu.fit_text(protocol);
        let diff = open.menu.diff(&menu);
        // The client's field holds the open menu's text.
        let refill = anvil && open.menu.text() != menu.text();
        if anvil && (diff.title || refill || diff.slots.contains(&ANVIL_LEFT)) {
            menu.field_from_text();
        }
        let old = (
            std::mem::replace(&mut open.menu, menu),
            std::mem::replace(&mut open.shown, shown),
            std::mem::take(&mut open.stacks),
        );
        if let Err(error) = open.build_stacks(protocol) {
            log_warn!(
                "menu {} of {}: not redrawn: {error}",
                open.menu.id(),
                describe(&open.owner)
            );
            (open.menu, open.shown, open.stacks) = old;
            return Ok(());
        }
        // Every stack is built for the current text.
        open.result_stale = false;
        let mut sent = Vec::new();
        let slots: Vec<usize> = if diff.title {
            sent.push(packets::open_screen(
                version,
                open.container,
                open.menu.kind(),
                open.menu.title(),
            )?);
            (0..open.slots())
                .filter(|slot| open.menu.slots()[*slot].is_some())
                .collect()
        } else {
            // A slot that only does something else looks the same, but is
            // sent too: the client then names the state that showed it.
            let mut slots = diff.slots.clone();
            slots.extend(&diff.actions);
            if refill {
                slots.push(ANVIL_LEFT);
            }
            if anvil
                && slots
                    .iter()
                    .any(|slot| [ANVIL_LEFT, ANVIL_RIGHT].contains(slot))
            {
                slots.push(ANVIL_RESULT);
            }
            slots.sort_unstable();
            slots.dedup();
            slots
        };
        if !slots.is_empty() || diff.title {
            let state = open.next_state();
            // A left item that looks as before would not refill the field:
            // the client sees it change through an empty slot.
            if refill && !diff.title && open.stacks.get(ANVIL_LEFT) == old.2.get(ANVIL_LEFT) {
                let empty = menu::stack::encode(protocol, None)?;
                sent.push(packets::set_slot(
                    version,
                    open.container,
                    state,
                    ANVIL_LEFT as i16,
                    &empty,
                )?);
            }
            for slot in slots {
                sent.push(packets::set_slot(
                    version,
                    open.container,
                    state,
                    slot as i16,
                    &open.stacks[slot],
                )?);
            }
            if anvil {
                sent.push(packets::anvil_cost(version, open.container)?);
            }
        }
        for slot in diff.actions {
            if let Some(changed) = open.changed.get_mut(slot) {
                *changed = open.updates;
            }
        }
        for packet in &sent {
            self.client.inject(packet)?;
        }
        Ok(())
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

    /// A menu that cannot reach the player now, because they are between
    /// servers: its owner gets the `switch` ending at once.
    pub fn menu_undeliverable(&mut self, shown: &Shown) {
        log_debug!(
            "menu {} of {}: not shown: the player is not in a world",
            shown.menu.id(),
            describe(&shown.owner)
        );
        self.menus.refuse(shown, CloseReason::Switch);
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

    /// The [`Menus::deadline`] passed: send the inventory request that
    /// waited, or report the text the player typed last.
    pub(super) fn menu_timer(&mut self, version: ProtocolVersion) -> io::Result<SessionEvent> {
        let now = Instant::now();
        if self.menus.request.due().is_some_and(|due| due <= now) {
            match self.backend.as_mut() {
                Some(backend) => self.menus.request_inventory(version, backend)?,
                None => self.menus.request.again = false,
            }
        }
        if let Some(open) = self.menus.open.as_mut()
            && open.result_due().is_some_and(|due| due <= now)
        {
            open.fresh_result(now, version.number())?;
            let state = open.next_state();
            let result = packets::set_slot(
                version,
                open.container,
                state,
                ANVIL_RESULT as i16,
                &open.stacks[ANVIL_RESULT],
            )?;
            self.client.inject(&result)?;
            self.client
                .inject(&packets::anvil_cost(version, open.container)?)?;
        }
        let undelivered = self.menus.undelivered.load(Ordering::Relaxed);
        if let Some(open) = self.menus.open.as_mut()
            && open.text_due().is_some_and(|due| due <= now)
        {
            open.typed_reported(now);
            if undelivered < menu::MAX_PENDING_EVENTS {
                let event = MenuEvent::text(&open.menu);
                let owner = open.owner.clone();
                return Ok(SessionEvent::Menu(self.menus.delivery(owner, event)));
            }
        }
        Ok(SessionEvent::Packet)
    }

    /// Write what Rift queued for the client now. Cancel-safe: what is not
    /// written stays queued for the next forward.
    pub(super) async fn flush_injected(&mut self) -> io::Result<()> {
        poll_fn(|cx| self.client.poll_replies(cx)).await
    }

    /// A serverbound packet in play: Rift's when it names Rift's container,
    /// is a creative slot while a menu is open, or can only come from a
    /// screen of the game's while Rift's menu is open. `None` passes it on.
    pub(super) fn menu_client_packet(
        &mut self,
        version: ProtocolVersion,
        packet: &Packet,
    ) -> io::Result<Option<SessionEvent>> {
        let Some(ids) = self.menus.ids(version) else {
            return Ok(None);
        };
        // A client in creative mode would create the item it drops, also
        // just after Rift closed the menu, before the client knew.
        let grace = self
            .menus
            .rift_ended
            .is_some_and(|ended| ended.elapsed() < CREATIVE_GRACE);
        if (self.menus.open.is_some() || grace) && packets::creative(&ids, packet) {
            return Ok(Some(SessionEvent::Packet));
        }
        // An anvil's text names no container: while Rift's anvil is open,
        // or was the last screen, it is Rift's.
        let anvil = self.menus.open.as_ref().is_some_and(OpenMenu::anvil);
        let late = self.menus.open.is_none()
            && self.menus.late_anvil
            && self.menus.backend_container.is_none();
        if (anvil || late)
            && let Some(name) = packets::rename(&ids, packet)?
        {
            return Ok(Some(if anvil {
                self.menu_rename(version, &name)?
            } else {
                SessionEvent::Packet
            }));
        }
        // Only a screen of the game's could have sent it, and Rift's menu
        // is what the client shows: a modified client's, or a late one.
        if self.menus.open.is_some() && packets::screen_only(&ids, packet) {
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
        if let Some(slots) = self.menus.earlier(container) {
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

    /// The player changed the text of Rift's anvil, or the client sent it
    /// again (when the left slot is set, and on 1.12 on every key). The
    /// client computed a result and a cost of its own for it, whatever it
    /// is, so those are sent again each time; a new text is remembered, and
    /// reported to a menu with `typing`.
    fn menu_rename(&mut self, version: ProtocolVersion, name: &str) -> io::Result<SessionEvent> {
        let protocol = version.number();
        // Longer than the client allows: the game ignores it too, and a
        // vanilla client never sends one.
        let Some(text) = menu::anvil_rename(protocol, name) else {
            return Ok(SessionEvent::Packet);
        };
        let undelivered = self.menus.undelivered.load(Ordering::Relaxed);
        let Some(open) = self.menus.open.as_mut() else {
            return Ok(SessionEvent::Packet);
        };
        let changed = open.menu.text() != text;
        let now = Instant::now();
        if changed {
            open.menu.typed(text);
            open.typed_result(now, protocol)?;
        }
        let state = open.next_state();
        let result = packets::set_slot(
            version,
            open.container,
            state,
            ANVIL_RESULT as i16,
            &open.stacks[ANVIL_RESULT],
        )?;
        self.client.inject(&result)?;
        self.client
            .inject(&packets::anvil_cost(version, open.container)?)?;
        if changed
            && open.menu.typing()
            && open.typed_now(now)
            && undelivered < menu::MAX_PENDING_EVENTS
        {
            let event = MenuEvent::text(&open.menu);
            let owner = open.owner.clone();
            return Ok(SessionEvent::Menu(self.menus.delivery(owner, event)));
        }
        Ok(SessionEvent::Packet)
    }

    /// A click on the open menu: undo what the client predicted, ask for the
    /// inventory if its rows were touched, and report the click if the
    /// button reports this kind and the player saw the action they clicked.
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
        let mut report = None;
        let mut close = false;
        if click.on_menu(slots)
            && let Some((kind, key)) = click.kind()
            && let Some(button) = open.menu.button(i32::from(click.slot))
        {
            let slot = click.slot as usize;
            // What the slot does changed after what the player saw.
            if !open.stale(slot, click.state_id) {
                if button.action.reports(kind) && undelivered < menu::MAX_PENDING_EVENTS {
                    report = Some((
                        open.owner.clone(),
                        MenuEvent::click(&open.menu, slot, kind, key),
                    ));
                }
                close = button.action.closes(kind);
            }
        }
        let anvil = open.anvil();
        let mut sent = click.menu_slots(slots);
        if anvil {
            // Every input may have changed on the client, and with them its
            // own result and cost; a click on the result that raced a
            // keystroke may have taken the cost off the levels it shows and
            // emptied the inputs. The left item is named after the current
            // text, so the field it fills again keeps what the player typed.
            if open.menu.field() != open.menu.text() {
                open.menu.field_from_text();
                open.rebuild(ANVIL_LEFT, version.number())?;
            }
            open.fresh_result(Instant::now(), version.number())?;
            sent = vec![ANVIL_LEFT, ANVIL_RIGHT, ANVIL_RESULT];
        }
        let state = open.next_state();
        for slot in sent {
            let stack = packets::set_slot(
                version,
                open.container,
                state,
                slot as i16,
                &open.stacks[slot],
            )?;
            self.client.inject(&stack)?;
        }
        if anvil {
            self.client
                .inject(&packets::anvil_cost(version, open.container)?)?;
            if usize::try_from(click.slot) == Ok(ANVIL_RESULT)
                && let Some(experience) = &self.menus.experience
            {
                self.client.inject(experience)?;
            }
        }
        self.client
            .inject(&packets::empty_cursor(version, state)?)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_patch_that_does_not_fit_names_the_plugin_the_menu_and_why() {
        let update = |owner| Update {
            owner,
            id: "shop".into(),
            patch: Arc::new(menu::Patch::parse(&serde_json::json!({"text": "x"})).unwrap()),
            heads: Arc::default(),
        };
        let mut chest = Menu::parse(&serde_json::json!({"id": "shop"})).unwrap();
        let error = chest.apply(&update(Owner::Script).patch).unwrap_err();
        assert_eq!(
            patch_not_applied(&update(Owner::Plugin("shops".into())), &error),
            "menu shop of plugin shops: patch not applied: patch.text is for anvils only"
        );
        assert_eq!(
            patch_not_applied(
                &update(Owner::Script),
                "slot 27: a chest with 3 rows has slots 0 to 26"
            ),
            "menu shop of rift.lua: patch not applied: slot 27: a chest with 3 rows has slots 0 to 26"
        );
        let remote = Owner::Remote {
            peer: 2,
            incarnation: 1,
            plugin: Some("shops".into()),
        };
        assert!(
            patch_not_applied(&update(remote), "x")
                .starts_with("menu shop of plugin shops of peer 2:")
        );
    }

    #[test]
    fn state_ids_wrap_and_name_the_latest_update_with_them() {
        assert_eq!(state_id(0), 0);
        assert_eq!(state_id(1), 1);
        assert_eq!(state_id(MAX_STATE_ID as u64), MAX_STATE_ID);
        assert_eq!(state_id(MAX_STATE_ID as u64 + 1), 1);
        assert_eq!(seen(5, 5), 5);
        assert_eq!(seen(5, 3), 3);
        // An ID Rift never sent, 0, and garbage name no update.
        assert_eq!(seen(5, 0), 0);
        assert_eq!(seen(5, -4), 0);
        assert_eq!(seen(5, MAX_STATE_ID + 1), 0);
        assert_eq!(seen(5, 6), 0);
        // Across the wrap.
        let updates = MAX_STATE_ID as u64 + 2;
        assert_eq!(state_id(updates), 2);
        assert_eq!(seen(updates, 2), updates);
        assert_eq!(seen(updates, 1), updates - 1);
        assert_eq!(seen(updates, MAX_STATE_ID), updates - 2);
    }
}
