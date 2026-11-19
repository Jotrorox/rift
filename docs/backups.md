# Backups

Rift backs up the persistent instances of service groups on
[nodes](nodes.md) on a schedule, to a folder on the proxy host or to an S3
bucket, and restores them with one command, onto their own node or onto
another one when theirs is gone.

```lua
rift.config.backups = {
    interval_ms = 6 * 60 * 60 * 1000, -- every 6 hours (the default)
    keep = 7,                          -- snapshots per instance (the default)
}
```

Without `directory` or `s3`, backups go to `local/backups` next to the
configuration. Every persistent instance of a group with `nodes` is backed up
once `backups` is set. Instances on this host are not: their folders are on
the proxy host already, so back them up with the host.

## Settings

| Setting | Default | Meaning |
| --- | --- | --- |
| `directory` | `local/backups` | A folder on the proxy host. Relative paths start at the configuration's folder |
| `s3` | none | An [S3 bucket](#back-up-to-s3) instead of a folder |
| `interval_ms` | 21600000 (6 h) | How old an instance's newest snapshot may get before Rift takes the next one, from 60000 (1 min) to 30 days. `0` backs up only on request |
| `keep` | 7 | Snapshots kept per instance, 1 to 1000. Older ones are deleted after each backup |
| `groups` | all | The persistent groups on nodes to back up on schedule, for example `{ "survival" }` |
| `save_commands` | `true` | Flush a running server's world before copying it; see [how a backup works](#how-a-backup-works) |

## How a backup works

1. If the server is running, the agent sends `save-off` and
   `save-all flush` to its console and waits up to 60 s for the server to log
   `Saved the game`. Without that line the backup fails rather than copy a
   world in the middle of a save. Set `save_commands = false` for servers that
   lack these commands.
2. The agent copies the instance folder into a snapshot folder next to it
   (`local/services/.rift-backup-<instance>`) and takes the SHA-256 of every
   file on the way, then sends `save-on`. Saving is paused only for this
   local copy, not for the upload. The node needs as much free disk as the
   folder takes, plus 256 MiB; the agent refuses the snapshot otherwise.
3. The proxy reads the snapshot over the message bus, in chunks of 512 KiB,
   and stores each file under its SHA-256. A file that an earlier snapshot
   already stored is not read again, so after the first backup only changed
   files cross the network, while every snapshot stays complete on its own.
4. It writes the snapshot's manifest, has the agent delete the snapshot
   folder, and deletes snapshots beyond `keep` together with the files no
   snapshot needs any more.

Symlinks and other special files are left out and listed in the reply; a
restore does not bring them back. Players stay connected throughout.

Rift runs one backup or restore at a time. The schedule looks for due
instances every minute, skips instances whose node is offline, and retries a
failed backup after 15 minutes at the earliest. A failure is logged as a
warning and shown by `rift admin backups`.

## Back up to S3

```lua
rift.config.backups = {
    s3 = {
        bucket = "rift-backups",
        region = "eu-central-1",
        prefix = "network-1/",
    },
}
```

| Setting | Default | Meaning |
| --- | --- | --- |
| `bucket` | required | The bucket name |
| `region` | `us-east-1` | The region the requests are signed for |
| `endpoint` | AWS for the region | `https://host[:port]` of an S3-compatible service, such as MinIO, Cloudflare R2, Hetzner or Backblaze B2. `http://` only on loopback |
| `prefix` | none | Prepended to every key, so several networks can share a bucket |
| `path_style` | `true` with `endpoint` | Name the bucket in the path (`endpoint/bucket/key`) instead of the host name |
| `access_key_env` | `RIFT_BACKUP_ACCESS_KEY` | Environment variable with the access key ID |
| `secret_key_env` | `RIFT_BACKUP_SECRET_KEY` | Environment variable with the secret access key |

The credentials need to list, read, write and delete objects below the
prefix. Rift keeps both variables out of every server it starts, and
`rift check --strict` reports them if they are unset. Files over 16 MiB are
uploaded in parts.

## Layout

A folder and a bucket hold the same layout:

```text
objects/<first two hex digits>/<sha256>   file contents
snapshots/<instance>/<snapshot>.json      one manifest per snapshot
```

A snapshot is named after its UTC time, such as `20261118T123456Z`. Its
manifest lists every folder and file of the instance with its size,
permissions and SHA-256. Copy the whole folder or bucket to move or mirror
your backups; never delete single objects, since several snapshots share
them.

## Commands

Each command needs the `servers` [admin permission](admin.md) and returns
when it has finished.

| Command | What it does |
| --- | --- |
| `rift admin backup <instance>` | Takes a snapshot now, also with `interval_ms = 0` |
| `rift admin backups` | Lists the backed-up instances with their number of snapshots, the newest one and the last attempt |
| `rift admin backups <instance>` | Lists one instance's snapshots with their size, node and whether the world was flushed |
| `rift admin backups forget <instance> <snapshot>\|all` | Deletes snapshots, also of instances that no longer exist |
| `rift admin restore <instance> [<snapshot>\|latest] [--node <node>]` | [Restores an instance](#restore-an-instance) |

Removing an instance keeps its snapshots. Delete them with `backups forget`
once you no longer need them.

## Restore an instance

```sh
rift admin restore survival-1
rift admin restore survival-1 20261118T123456Z
```

Rift stops the instance and refuses every start until the restore ends, as
for a [move](nodes.md#move-a-persistent-instance-to-another-node), so players
must have left first. It sends the snapshot's files to a staging folder next
to the instance folder, where the agent checks each against its SHA-256, and
then swaps the restored folder for the old one, which it deletes. It updates
the port in `server.properties` if the instance got another one since the
snapshot, and starts the instance again if it was running.

If anything fails before the swap, the old folder stays untouched and the
instance starts again as it was.

### Restore onto another node

With `--node`, the snapshot goes to another node and the instance moves
there, exactly as `rift admin move` would move it: the node must match the
group's `nodes`, be online and have room, and the instance gets a new backend
address. The old node is never contacted, so this works when it is gone for
good:

```sh
rift admin restore survival-1 --node node3
```

This is how you [recover the instances of a dead node](nodes.md#recover-the-instances-of-a-dead-node).
Keep the dead node in `rift.config.nodes` until its instances have moved. If
it comes back, its agent stops any server that no longer belongs to it, but
leaves the old folder for you to delete.

## Upgrading

Backups and restores need agents with the `backup` feature; Rift logs a
warning once per node whose agent lacks it and backs up nothing there. A
restore onto another node needs the `move` feature on that node instead.
Upgrade the controller first, then the agents ([upgrade nodes](upgrading.md#upgrade-nodes)).
