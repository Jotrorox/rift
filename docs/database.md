# Instance database: SQLite, PostgreSQL or MariaDB

Rift keeps its durable state in one database, the **instance database**:

- instance registrations, login generations, group states and node
  schedules ([service groups](service-groups.md), [nodes](nodes.md));
- the [permission store](permissions.md) and its audit log;
- the [player directory](players-api.md): names, last seen, languages;
- with `extensions.storage.database = true`, the
  [extension store](extensions.md#storage).

By default this is the SQLite file `instance_database` (`rift.sqlite3` beside
the configuration). It needs no setup and suits a single host. Rift can keep
the same tables on a database server instead, PostgreSQL or MariaDB. Every
backend behaves the same: the same features, the same limits, the same
`rift check --strict` coverage.

| | SQLite (default) | PostgreSQL | MariaDB |
| --- | --- | --- | --- |
| Setup | None | A database and a role | A database and a user |
| Where the data lives | A file on the controller's disk | A database server, reachable from any host | A database server, reachable from any host |
| Backups | Copy the file while Rift is stopped | `pg_dump` while Rift runs, or the server's own backups and replicas | `mariadb-dump --single-transaction` while Rift runs, or the server's own backups and replicas |
| A second controller | Not possible alone; a [cluster](clustering.md) replicates SQLite to several active proxies | A [standby](#standby-controllers) that takes over when the first stops | The same |
| Inspect or report on the data | `sqlite3 rift.sqlite3` | Any PostgreSQL client or BI tool, from anywhere | Any MariaDB or MySQL client or BI tool, from anywhere |

Choose a server when you run [several hosts](nodes.md) and want the
controller's state off its own disk, want a standby controller, or already
run one with backups and replication. Pick whichever of the two you already
operate; Rift works the same on both.

A [cluster](clustering.md) of several active proxies keeps SQLite instead: it
replicates the instance database to every peer with Raft and needs no
database server. It cannot use PostgreSQL or MariaDB.

## Use PostgreSQL

1. Create a role and a database for Rift (PostgreSQL 13 or newer):

   ```sql
   CREATE ROLE rift LOGIN PASSWORD 'change-me';
   CREATE DATABASE rift OWNER rift;
   ```

   Rift creates its tables at the first start. It needs no superuser rights:
   owning the database (or the [schema](#settings)) is enough.

2. Put the connection URL in Rift's environment, never in `rift.lua`:

   ```sh
   RIFT_DATABASE_URL='postgres://rift:change-me@db.internal:5432/rift?sslmode=require'
   ```

3. Select the backend:

   ```lua
   rift.config.database = { backend = "postgres" }
   ```

4. Check it, then start Rift:

   ```sh
   rift check --strict rift.lua
   ```

   The strict check connects read-only and reports whether Rift's tables
   exist yet, whether their schema version is supported, and whether the
   registered instances fit the configuration. It changes nothing.

To move data you already have, see [copy between backends](#copy-between-backends).

## Use MariaDB

1. Create a database and a user for Rift (MariaDB 10.5 or newer; MySQL is
   not supported, it lacks `INSERT ... RETURNING`):

   ```sql
   CREATE DATABASE rift CHARACTER SET utf8mb4 COLLATE utf8mb4_nopad_bin;
   CREATE USER 'rift'@'%' IDENTIFIED BY 'change-me';
   GRANT ALL PRIVILEGES ON rift.* TO 'rift'@'%';
   ```

   Rift creates its tables at the first start, each with the bytewise
   `utf8mb4_nopad_bin` collation whatever the database's default is. All
   privileges on its own database are enough. To let Rift create the
   database named by [`schema`](#settings) itself, grant `CREATE` on it too.

2. Put the connection URL in Rift's environment, never in `rift.lua`:

   ```sh
   RIFT_DATABASE_URL='mariadb://rift:change-me@db.internal:3306/rift?require_ssl=true'
   ```

3. Select the backend:

   ```lua
   rift.config.database = { backend = "mariadb" }
   ```

4. Check it with `rift check --strict rift.lua`, then start Rift, as for
   PostgreSQL.

The [settings](#settings) are the same as for PostgreSQL. `schema` names a
database on the server instead of the URL's, created when missing, so
several Rift networks can share one server.

### The MariaDB URL

The URL takes the form `mariadb://user:password@host:port/database?options`
(`mysql://` works too). Useful options:

| Option | Effect |
| --- | --- |
| `require_ssl=true` | Use TLS, verifying the server's certificate and name against the public CAs and `database.ca`. Without it (and without `database.ca`), the connection is not encrypted |
| `verify_identity=false` | With TLS, accept a certificate that does not name the host (only on a trusted network) |
| `socket=/run/mysqld/mysqld.sock` | Connect through this Unix socket |
| `compression=fast` | Compress the protocol |

Setting `database.ca` turns TLS on by itself. Rift runs every session with
`ANSI_QUOTES`, `PIPES_AS_CONCAT`, `NO_BACKSLASH_ESCAPES` and strict tables,
whatever the server's `sql_mode` is.

### Settings

```lua
rift.config.database = {
    backend = "postgres",
    url_env = "RIFT_DATABASE_URL", -- the default
    ca = "certs/postgres-ca.pem",  -- optional
    schema = "rift",               -- optional
    standby = false,               -- the default
}
```

| Field | Default | Meaning |
| --- | --- | --- |
| `backend` | required | `"postgres"`, `"mariadb"`, or `"sqlite"` for the default file (then no other field is allowed; the file is `instance_database`) |
| `url_env` | `RIFT_DATABASE_URL` | Environment variable holding the connection URL. Read once at startup; servers Rift starts never inherit it |
| `url` | none | The URL itself, instead of `url_env`. Use it only for a URL without a password (a server that trusts Rift's Unix socket peer, for example), since `rift.lua` is not a secret |
| `ca` | none | PEM file with certificates to trust for TLS, in addition to the public CAs. Relative to the configuration file |
| `schema` | the role's `search_path`; the URL's database on MariaDB | Keep Rift's tables in this schema (on MariaDB: this database), created when missing. Lets several Rift networks share one database or server: give each its own schema |
| `standby` | `false` | Wait for the controller that owns the database instead of failing; see [standby controllers](#standby-controllers) |

Changing any of these needs a restart; a reload that changes them is refused.

### The PostgreSQL URL

The URL takes the libpq form `postgres://user:password@host:port/database?options`.
Useful options:

| Option | Effect |
| --- | --- |
| `sslmode=disable`, `prefer` (default), `require` | Whether TLS is used. With TLS, Rift always verifies the server's certificate and name against the public CAs and `database.ca` |
| `connect_timeout=10` | Seconds to wait for a connection (default 10) |
| `target_session_attrs=read-write` | With several hosts (`postgres://rift@db1,db2/rift?...`), Rift connects to the writable primary, so it follows a PostgreSQL failover when it reconnects |
| `options=-c%20statement_timeout%3D30s` | Server settings for Rift's session |

A Unix socket works too: `postgres://rift@%2Fvar%2Frun%2Fpostgresql/rift`.

> [!IMPORTANT]
> With `sslmode=prefer` and a server that offers TLS with a certificate Rift
> cannot verify (for example a self-signed one), the connection fails. Give
> the server's CA as `database.ca`, or use `sslmode=disable` on a trusted
> network.

## One owner at a time

Exactly one Rift writes to a database, as with the SQLite file and its
`.lock`. For PostgreSQL, the owning Rift holds a session-level advisory lock
for its schema; for MariaDB, a named lock (`GET_LOCK`) for its database. A
second Rift pointed at the same database and schema refuses to start:

```text
rift: instance database postgres://rift@db.internal:5432/rift: already in use by rift/1f2a-… from 10.0.0.2; another Rift owns this database (set database.standby = true to wait for it)
```

On MariaDB the owner is named by its connection id (`connection 42`), with
its account and address when Rift's user may see other connections (the
`PROCESS` privilege).

Nodes never connect to the database; only the controller does.

When the owner's connection drops (a network blip, a database restart), Rift
reconnects and takes its lock back. Statements during the outage fail and are
logged; a write in progress is rolled back and reported to whoever asked for
it. If, by the time it reconnects, **another** Rift holds the lock, the old
owner has been replaced. It logs this and exits immediately, so two
controllers never act on the same data:

```text
rift: error: database postgres://rift@db.internal:5432/rift is now owned by rift/77c1-… from 10.0.0.3: another Rift took over while the connection was down; exiting so two Rifts never write at once
```

## Standby controllers

With `standby = true`, a Rift that finds the database owned waits, retrying
every second, and starts once the owner's session ends:

```text
rift: standing by: database postgres://rift@db.internal:5432/rift is owned by rift/1f2a-… from 10.0.0.2; starting once it is released
rift: took over database postgres://rift@db.internal:5432/rift
```

Run a second controller on another host with the same configuration and
`standby = true`. When the active controller stops or its host dies,
the server ends its session and the standby takes the lock and starts: it
restores the registered instances, and [node agents](nodes.md) keep their
servers running and are adopted again once they reach the new controller.

A standby is a cold spare, not a second active proxy. For proxies that all
accept players, and lose only their own players when one fails, run a
[cluster](clustering.md#cluster-or-standby) instead. Plan for these:

- **One address.** Players, node agents (`--controller`, `--web`) and
  backends connect to one controller. Move a floating IP or DNS name to the
  standby when it takes over (keepalived, a cloud load balancer health check
  on the game port, or similar). The messaging certificate must name that
  address on both hosts.
- **Detection time.** A clean stop releases the lock at once. When the
  active host vanishes, the server notices only when its TCP connection
  times out. On PostgreSQL, lower `tcp_keepalives_idle`,
  `tcp_keepalives_interval` and `tcp_keepalives_count` on the server to take
  over within seconds rather than hours. MariaDB ends a silent connection
  after `wait_timeout` (8 hours by default; Rift pings every 30 seconds, so
  a lower value such as 120 is safe) or when the operating system's TCP
  keepalive gives up.
- **Files beside the configuration stay per host.** Templates
  (`template_storage`), the software cache, the `rift.perms-changes/`
  permission change stream and an extension store kept in a file are not in
  the database. Keep templates on shared storage or sync them, and use
  `extensions.storage = { database = true }`. Backends refetch permission
  snapshots from the new controller, so the change stream may start over.
- **Same secrets.** Node tokens, the forwarding secret and messaging
  principals must be the same on both hosts.

## Copy between backends

`rift database copy` copies every Rift table between a SQLite file and the
PostgreSQL or MariaDB database `rift.config.database` names. Stop Rift first:
both databases are opened exclusively.

```sh
# From the configured instance_database file into the server:
rift database copy rift.lua
# From another file:
rift database copy --from /var/backups/rift.sqlite3 rift.lua
# Back from the server into a new SQLite file:
rift database copy --to rift.sqlite3 rift.lua
```

To move between PostgreSQL and MariaDB, go through a SQLite file: copy
`--to` a file with one backend configured, then copy `--from` that file with
the other.

- Both sides are brought to the current schema first, as startup would.
- Rows keep their ids, so permission revisions continue where they were.
- A target that already holds data is refused; `--replace` deletes its rows
  first. Everything happens in one transaction on the target.
- Then set (or remove) `rift.config.database` and start Rift.

To switch to a server:

1. Stop Rift and keep a copy of `rift.sqlite3`.
2. Add `rift.config.database = { backend = "postgres" }` (or `"mariadb"`)
   and set `RIFT_DATABASE_URL`.
3. Run `rift database copy rift.lua`, then `rift check --strict rift.lua`.
4. Start Rift.

To switch back, run `rift database copy --to rift.sqlite3 rift.lua`, remove
`rift.config.database`, and start Rift.

## Backups and upgrades

- Back up with `pg_dump` or `mariadb-dump --single-transaction` (or the
  server's continuous backups); unlike the SQLite file, this works while Rift
  runs and gives a consistent snapshot.
- A new release migrates Rift's tables in place at its first start, in one
  transaction per component, and an older release refuses a newer schema
  (`unsupported schema version N of <component>`). MariaDB commits table
  changes immediately, so a migration it interrupts can leave some of a
  component's new tables behind; restore the dump before trying again. Take
  a dump before upgrading; rolling back means restoring it. See
  [upgrade and rollback](upgrading.md).
- Rift's tables are `rift_schema`, `instances`, `sequences`,
  `login_generations`, `group_states`, `node_schedules`, `players`, the
  `perm_*` tables and, with database storage, `extension_entries`. Treat them
  as Rift's: change them through Rift, not by hand, while it runs.
