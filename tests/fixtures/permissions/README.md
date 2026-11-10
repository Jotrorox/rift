# Permission fixture corpus

Every `*.json` file here is one language-neutral test case for Rift
permissions. The Rust crate (`crates/rift-permissions/tests/fixtures.rs`), the
Java client and any other port run the whole directory; a port may skip kinds
it does not implement (for example a backend that has no LuckPerms importer),
but must not skip a file of a kind it does implement. The JSON shapes are the
ones in [`docs/permissions-protocol.md`](../../../docs/permissions-protocol.md).

Every file is an object with:

| Field | Meaning |
| --- | --- |
| `kind` | `resolve`, `wire` or `luckperms` |
| `name` | The file name without the `<kind>-` prefix and `.json` |
| `description` | Optional prose |

## `resolve`: compile and check

```json
{
  "kind": "resolve",
  "name": "context-pairs-beat-rank",
  "now": 1000,
  "users": {"<uuid>": {"primary_group": "vip", "nodes": [<node>, ...]}},
  "groups": {"<name>": {"weight": 0, "display_name": "VIP", "nodes": [<node>, ...]}},
  "checks": [{"user": "<uuid>", "node": "fly", "contexts": {"server": "survival"}, "now": 1000, "expect": true}],
  "meta": [{"user": "<uuid>", "contexts": {}, "now": 1000, "expect": <meta view>}],
  "compiled": {"<uuid>": <compiled subject>}
}
```

- `users` maps a UUID (32 lowercase hex digits) to the user's optional
  `primary_group` and `nodes` (typed node JSON, possibly with uppercase
  that the reader must normalise). Only these users are checked.
- `groups` maps a group name to `weight` (default 0), optional
  `display_name` and `nodes`. `default` may be absent; it is still the
  implicit last parent of every user and contributes nothing then.
- `now` is the Unix time used for compilation and, unless a check or meta
  expectation sets its own `now`, for evaluation.
- `checks[].expect` is `true`, `false` or `null` (undefined). `contexts`
  defaults to `{}`. `node` is passed to `check()` as written: the
  implementation lowercases it, and an invalid node is `null`.
- `meta[].expect` is a meta view: `{"prefix", "suffix", "meta", "primary_group"}`,
  `null` for a missing prefix or suffix.
- `compiled` is the exact Stage 1 output for every user, built at `now` with
  revision 1. A port that implements compilation must produce it exactly
  (entry order included); a port that implements only Stage 2 (the Java
  client) runs `checks` and `meta` against these subjects instead of
  compiling.

Expiry is exclusive: a node or entry with `expires_at` (or `exp`) `t` applies
while `now < t`.

The `compiled` member is generated. After changing users, groups or the
compiler, run

```sh
RIFT_BLESS_FIXTURES=1 cargo test -p rift-permissions --test fixtures
```

and review the diff. Blessing rewrites only the trailing `compiled` member, so
keep it last and keep top-level members indented by two spaces.

## `wire`: message round trip

```json
{"kind": "wire", "name": "hello", "type": "hello_request", "json": {...}, "expect": {...}}
```

Parse `json` as `type`, serialise the result again, and compare it with
`expect` as JSON values (member order does not matter). Without `expect` the
output must equal `json`. With `"error": true` parsing must fail instead.
`change` and `changes` must additionally pass `Change::normalized`.

| `type` | Shape |
| --- | --- |
| `compiled_subject` | compiled subject |
| `node`, `nodes` | typed node, array of typed nodes |
| `context_set` | context set |
| `change`, `changes` | change, array of changes |
| `change_event` | `rift.perms.update` payload |
| `applied` | `rift.perms.apply` reply data |
| `hello_request`, `hello_reply` | `rift.perms.hello` |
| `subject_get_request` | `rift.perms.subject.get` request |
| `subject_batch_request`, `subject_batch_reply` | `rift.perms.subject.batch` |
| `groups_get_request`, `groups_reply` | `rift.perms.groups.get` |
| `apply_request` | `rift.perms.apply` request |
| `register_request` | `rift.perms.register` payload |
| `command_request`, `command_reply` | `rift.perms.command` |
| `context_report` | `rift.perms.context` payload |
| `resync_request` | `rift.perms.update.resync` payload |
| `reply` | reply envelope with arbitrary `data` |
| `tristates` | array of `true`/`false`/`null` |
| `meta_view` | meta view |
| `trace` | trace result |

## `luckperms`: string node import

```json
{"kind": "luckperms", "name": "chat-meta", "node": {"type": "prefix", "key": "prefix.100.&6[VIP] ", "value": true}, "expect": [<node>, ...]}
```

Decode `node`, one entry of a `/lp export` `nodes` array, into typed nodes
with source `import` and compare with `expect` in order. With
`"error": "regex"` decoding must be refused as a regex node; with
`"error": "invalid"` it must fail for any other reason. An optional
`service_groups` lists the network's service groups (lowercase): a
`server=<name>` pair naming one of them becomes `group=<name>`, the reserved
context Rift fills with a player's service group.
