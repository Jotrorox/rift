# Permissions HTTP API

`/api/perms` reads and edits the [permission store](permissions.md) over HTTP,
on the [web service](http.md). The dashboard's Groups, Players, Nodes and
Activity views use nothing else. Use it to script groups, player nodes, tracks,
bulk edits and LuckPerms imports.

For example, give a player a temporary permission on one server, check it, and
move them up a track:

```sh
curl -s -X PUT http://127.0.0.1:8080/api/perms/users/Alice/nodes \
  -H "Authorization: Bearer $RIFT_WEB_TOKEN" -H 'Content-Type: application/json' \
  -d '{"kind": "permission", "key": "essentials.fly", "value": true,
       "contexts": {"server": "survival"}, "expires_at": 1762800000, "reason": "event"}'
# {"revision":414,"ops":1,"changed_users":["0123…"],"changed_groups":[],"id":"permission:essentials.fly@server=survival"}

curl -s -X POST http://127.0.0.1:8080/api/perms/trace \
  -H "Authorization: Bearer $RIFT_WEB_TOKEN" -H 'Content-Type: application/json' \
  -d '{"user": "Alice", "node": "essentials.fly", "contexts": {"server": "survival"}}'
# {"node":"essentials.fly","result":true,"winner":{"entry":{"k":"perm","key":"essentials.fly","v":true,"ctx":{"server":"survival"},"rank":0,"exp":1762800000},"specificity":"exact","outcome":"won"},"losers":[],"ranks":[{"rank":0,"subject":"user:0123…"},{"rank":1,"subject":"group:vip"}],…}

curl -s -X POST http://127.0.0.1:8080/api/perms/tracks/ranks/promote \
  -H "Authorization: Bearer $RIFT_WEB_TOKEN" -H 'Content-Type: application/json' \
  -d '{"user": "Alice"}'

# Remove the node again by its id.
curl -s -X DELETE 'http://127.0.0.1:8080/api/perms/users/Alice/nodes/permission%3Aessentials.fly%40server%3Dsurvival' \
  -H "Authorization: Bearer $RIFT_WEB_TOKEN" -H 'Content-Type: application/json' -d '{}'
```

## Make a request

- Reading needs the `perms` or `config`
  [web permission](http.md#authentication-and-access); writing needs
  `config`. `POST /api/perms/trace` only reads, so `perms` is enough for it.
  In the tables below, `perms` always means "`perms` or `config`".
- Writes take a JSON object (`Content-Type: application/json`), `{}` when
  there is nothing to send. Every write accepts an optional `reason` (at most
  256 bytes) that the change log keeps.
- Users in paths and bodies are a UUID, plain or dashed, or the name of a
  player the proxy has seen (online first, then the last-seen name). Every
  UUID is a valid user: one the store has never seen simply has no nodes.
- Shapes such as typed nodes, compiled subjects, changes and change events are
  those of the [permissions protocol](permissions-protocol.md).

[`rift dev`](permissions.md#rift-dev) serves this API and the dashboard on
`http://127.0.0.1:8080/` with its admin token as `web.token`, so plugin
authors can edit groups while a backend is connected.

## What a write does

Every write goes through the same single write path as `/perms`, Lua and the
bus. It is validated the same way, applies to the next check and is pushed to
backends.

- It creates one revision in the change log with actor `operator:<name>`: the
  web operator, `admin` for `web.token`, `local` for the generated token.
- It leaves an operator audit record (`GET /api/audit`) such as
  `PUT perms_user_nodes` with the user and node as target.
- A write that changes nothing creates no revision and returns `"ops": 0`.

Successful writes return the [applied](permissions-protocol.md#applied)
result, sometimes with extra members:

```json
{"revision": 413, "ops": 1, "changed_users": ["0123456789abcdef0123456789abcdef"], "changed_groups": []}
```

Errors carry the protocol's error code next to the message:

```json
{"error": "unknown group \"ghost\"", "code": "not_found"}
```

| Code | Status |
| --- | --- |
| `invalid_request` | `400` (also `413` for a body that is too large, `415` for a wrong content type) |
| `forbidden` | `403` |
| `not_found` | `404` |
| `conflict` | `409`, for example creating a group that exists or promoting past a track's last group |
| `limit` | `422`, a `permissions.limits` value would be exceeded |
| `unavailable` | `503` |
| `internal` | `500` |

Authentication and permission failures from the web service itself (`401`,
and `403` for a missing operator permission) carry no `code`.

## Node ids

A node in a path is named by its slot, `<kind>:<key>`, followed by
`@<contexts>` when it has contexts, with the canonical context string (pairs
sorted, `;` between them). The key is the protocol's storage key: the pattern,
group, priority or meta key.

- `permission:essentials.fly`
- `inherit:vip@server=survival`
- `prefix:100`
- `meta:homes@region=spawn;server=survival`

A subject holds one node per slot, so the id stays the same across edits and
restarts, and a client can build it from the typed node without asking.
Listings return each node's id. Percent-encode it as one path segment
(`permission%3Aessentials.fly%40server%3Dsurvival`).

## Store state

| Method and path | Permission | What it does |
| --- | --- | --- |
| `GET /api/perms/revision` | `perms` | `{revision}`: the last committed revision |
| `GET /api/perms/backends` | `perms` | `{revision, backends: [{principal, server, revision, hello_at, last_seen, client_version, requests, stale}]}`: every backend principal that has sent a `rift.perms.*` request since the proxy started |
| `GET /api/perms/registered` | `perms` | `{registered: [{owner, node, description, default, plugin}]}`: nodes backend plugins registered, sorted by node |
| `GET /api/perms/search?node=&like=&kind=&subjects=&source=&limit=` | `perms` | `{holders: [{subject, name, id, node}], truncated}`: who sets a node directly |

- **Backends.** A backend's `revision` is what its last `hello` reported. It
  is `stale` when that is ahead of the proxy (its store went back), or behind
  it and it has not said `hello` since. Times are Unix seconds.
- **Search.** `node` is an exact storage key, `like` a SQL `LIKE` pattern as
  in bulk filters; `subjects` is `any`, `users` or `groups`.

## Users

| Method and path | Permission | What it does |
| --- | --- | --- |
| `GET /api/perms/users?q=&group=&limit=` | `perms` | `{users: [{uuid, name, online, groups}], truncated}`: players in the store (everyone who joined or has nodes), sorted by name |
| `GET /api/perms/users/{user}` | `perms` | `{uuid, name, online, primary_group, effective_primary_group, tracks, contexts, nodes: [{id, node}], revision}` |
| `PATCH /api/perms/users/{user}` | `config` | `{primary_group}`: set the explicit primary group, `null` to clear it |
| `GET /api/perms/users/{user}/nodes` | `perms` | `{subject, nodes: [{id, node}], revision}` |
| `PUT /api/perms/users/{user}/nodes` | `config` | Set the typed node in the body, replacing the node in its slot; the result adds its `id` |
| `PUT /api/perms/users/{user}/nodes/{id}` | `config` | Replace that node with the body, which may move it to another slot (other contexts, say); `404` when no node has that id |
| `DELETE /api/perms/users/{user}/nodes/{id}` | `config` | Remove the node; `"ops": 0` when it was not there |
| `DELETE /api/perms/users/{user}/nodes` | `config` | Remove every node, or with `{contexts}` only those with exactly these contexts |
| `GET /api/perms/users/{user}/compiled?server=&world=&dimension=&gamemode=&...` | `perms` | `{subject, contexts, contexts_from, meta, groups}`: the compiled subject exactly as backends receive it, and the meta view and group memberships in the contexts given |

- **List.** `q` matches part of the name or the start of the UUID. `group`
  keeps those with a positive `inherit` node for it in any context. `groups`
  lists global parents.
- **One user.** `primary_group` is the explicit one or `null`; `contexts` are
  the player's current proxy contexts.
- **Compiled.** Every query parameter is a context pair (repeat a custom key
  for several values). With none, the player's current contexts are used and
  `contexts_from` is `proxy`.

## Groups

| Method and path | Permission | What it does |
| --- | --- | --- |
| `GET /api/perms/groups` | `perms` | `{revision, groups: [{name, weight, display_name, parents, nodes, tracks}]}`; `parents` are the group's `inherit` nodes, `nodes` a count |
| `POST /api/perms/groups` | `config` | Create `{name, weight?, display_name?}`; `201` with the result and `name` |
| `GET /api/perms/groups/{name}` | `perms` | `{name, weight, display_name, tracks, nodes: [{id, node}], revision}` |
| `PATCH /api/perms/groups/{name}` | `config` | `{weight?, display_name?}`, at least one; `display_name: null` clears it |
| `DELETE /api/perms/groups/{name}` | `config` | Delete a group and its nodes. `inherit` nodes naming it stay and grant nothing. `default` cannot be deleted |
| `GET`, `PUT`, `DELETE /api/perms/groups/{name}/nodes[/{id}]` | As for users | The same node routes as for users; `weight` and `display_name` nodes are allowed here |

## Tracks

| Method and path | Permission | What it does |
| --- | --- | --- |
| `GET /api/perms/tracks` | `perms` | `{revision, tracks: [{name, groups}]}` |
| `GET /api/perms/tracks/{name}` | `perms` | `{name, groups}` |
| `PUT /api/perms/tracks/{name}` | `config` | Create or replace with `{groups}`, lowest first, distinct |
| `DELETE /api/perms/tracks/{name}` | `config` | Delete a track; players keep their groups |
| `POST /api/perms/tracks/{name}/promote` | `config` | `{user, contexts?}`: move the player to the next group in exactly these contexts; the result adds `user` |
| `POST /api/perms/tracks/{name}/demote` | `config` | `{user, contexts?}`: move the player to the previous group |

## Change log and trace

| Method and path | Permission | What it does |
| --- | --- | --- |
| `GET /api/perms/audit?since=&actor=&subject=&limit=` | `perms` | `{revision, events}`: [change events](permissions-protocol.md#change-event) in revision order |
| `POST /api/perms/trace` | `perms` | `{user, node, contexts?}` → `{node, result, winner, losers, ranks, contexts, contexts_from, uuid, revision}`: the answer for the node, the winning entry and why every other matching entry lost |

- **Change log.** With `since`, the first `limit` events after that revision;
  otherwise the latest `limit` (default 100, at most 1000). `actor` is an
  actor string (`operator:alice`, `cli`); `subject` is `user:<uuid>`,
  `group:<name>`, a bare UUID or a group name.
- **Trace.** `ranks` maps each rank to `user:<uuid>` or `group:<name>`.
  Without `contexts`, the player's current ones are used.

## Bulk edits, import and export

| Method and path | Permission | What it does |
| --- | --- | --- |
| `POST /api/perms/apply` | `config` | `{change, reason?}`: apply one raw [change](permissions-protocol.md#change), any `op` including `batch` |
| `POST /api/perms/bulk` | `config` | `{filter, patch, dry_run?, reason?}`: a bulk edit in one revision. With `dry_run: true` nothing is written and the reply is `{dry_run: true, count, ops}`; otherwise the result with `dry_run: false` |
| `POST /api/perms/import?replace=` | `config` | Import a LuckPerms `/lp export` file sent as the raw body, JSON (`application/json`) or gzipped (`application/gzip`), at most 32 MiB. Returns `{revision, ops, groups, users, tracks, skipped}`. Additive; `replace=true` first deletes earlier imported nodes |
| `GET /api/perms/export?gzip=` | `perms` | The whole store in the LuckPerms export format; with `gzip` a gzipped download named `luckperms-export.json.gz` |

The import and export routes are the same as `/perms import` and `/perms
export` and use the format described under
[LuckPerms import and export](permissions-commands.md#luckperms-import-and-export).

Create a group with a prefix, preview a bulk edit, and move a LuckPerms
installation over:

```sh
api() { curl -s -H "Authorization: Bearer $RIFT_WEB_TOKEN" -H 'Content-Type: application/json' "$@"; }
api -X POST http://127.0.0.1:8080/api/perms/groups -d '{"name": "vip", "weight": 10, "display_name": "VIP"}'
api -X PUT http://127.0.0.1:8080/api/perms/groups/vip/nodes -d '{"kind": "prefix", "priority": 100, "text": "&6[VIP] "}'
api -X POST http://127.0.0.1:8080/api/perms/bulk -d '{"filter": {"kind": "permission", "key_like": "essentials.%"},
  "patch": {"value": false}, "dry_run": true}'
# {"dry_run":true,"count":3,"ops":[{"op":"node","subject":{"group":"vip"},"before":{…,"value":true},"after":{…,"value":false}},…]}
curl -s -X POST 'http://127.0.0.1:8080/api/perms/import' -H "Authorization: Bearer $RIFT_WEB_TOKEN" \
  -H 'Content-Type: application/gzip' --data-binary @luckperms-export.json.gz
# {"revision":420,"ops":312,"groups":6,"users":41,"tracks":1,"skipped":[{"subject":"group:mod","key":"r=^worldedit","reason":"…"}]}
curl -s 'http://127.0.0.1:8080/api/perms/export?gzip' -H "Authorization: Bearer $RIFT_WEB_TOKEN" -o rift-export.json.gz
```
