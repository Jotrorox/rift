# Server queues

A queue caps how many players a backend holds and makes the rest wait in
line. `queues = { survival = 100 }` in
[`rift.config.plugins`](extensions.md#set-up) sets a process-local capacity
of 100 for that backend. Queues require online authentication.

- The capacity counts initial attachments and transfers in progress as well as
  joined sessions. It does not count players connected directly to Paper or
  through another Rift process.
- All entry paths, including administrator transfers, reserve capacity
  atomically before opening the backend.
- During preflight the player holds the old and the target slot. Failure
  releases the target; successful world entry releases the old slot.

**Service groups.** For a group, the configured capacity is per registered
instance: `queues = { games = 20 }` allows 40 group slots with two instances.

- Queue order and leases apply across concrete instance names, so direct
  connections cannot bypass the group queue.
- Adding or removing instances updates capacity without changing tickets.
  Transfers within a group retain the player's existing group slot.
- A group with no instances can collect tickets, which its
  [scaling policy](service-groups.md#automatic-scaling) uses to start
  capacity.

**Tickets.** A queue command keeps the client playing on its current server.

- Each session can hold one ticket. Repeating the same request returns its
  current position; switching queues removes the old ticket only when the new
  request succeeds.
- Each queue allows 1024 tickets and a five-minute wait.
- `/queue leave` in the example cancels the ticket. Disconnects and successful
  transfers remove it.

**Admission.** The session checks its ticket once per second while it is in a
usable world.

- The queue head may enter when capacity is free. New direct entries cannot
  jump queued players, and one head completes entry before the next is
  considered.
- Admission rechecks permissions through `before_transfer`, static access and
  health.
- Exhausted capacity retains the ticket. Other transfer failures remove the
  ticket and tell the player; a partially completed protocol transition can
  require a disconnect.

## Priority

Tickets are ordered by priority, highest first, then by arrival. A player's
priority is the integer in their [meta](permissions.md#the-model) value
`queue_priority`, so a rank can carry it; `queue_priority.<destination>`
replaces it for one backend or service group. Unset, or not an integer, is 0,
and negative values go behind players without one:

```sh
/perms group vip meta set queue_priority 10
/perms group staff meta set queue_priority 100
/perms group vip meta set queue_priority.event 0   # no head start for the event server
```

- Meta resolves as usual: the player's own value, then their highest-weight
  group, in the contexts the proxy knows for them.
- A player who joins a queue later goes in front of tickets with a lower
  priority and behind those with the same or a higher one, and the reported
  position counts from there.
- The priority is read when the ticket is created; a rank change applies to the
  next ticket.
- With a steady supply of higher priorities, a low one may wait out its five
  minutes.

## Bypass

A player whose permission `rift.queue.bypass`, or
`rift.queue.bypass.<destination>` for one backend or service group, resolves to
`true` enters although the capacity is used up or others are queued: on the
initial join, with `/server` and on every transfer.

- A queue on a service group is bypassed with the group's node, also when the
  player targets one of its instances.
- Their slot still counts as used, so the destination stays over capacity until
  enough players leave, and queued players wait that much longer.
- Destination names are lowercased in the node. The node is checked like
  [server access](permissions.md#server-access), in the contexts the proxy
  knows for the player, and `*` or `rift.*` include it.
