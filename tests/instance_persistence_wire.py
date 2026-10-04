#!/usr/bin/env python3
"""SQLite registrations through real restarts and killed instance transactions."""

import argparse
from contextlib import closing
import json
import os
from pathlib import Path
import signal
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time

from managed_wire import SECRET, admin, eventually
from minecraft import ROOT, process, unused_port, wait_ready
from network_wire import Backend
from services_wire import api, available_range


def child(port, name, delay):
    with closing(Backend(name, port, port=port)):
        for line in sys.stdin:
            if line.strip() == "stop":
                Path("stop-requested.txt").write_text("stopping")
                time.sleep(delay)
                break


def check(binary):
    with tempfile.TemporaryDirectory(prefix="rift-instance-persistence-") as temporary:
        directory = Path(temporary).resolve()
        assets = directory / "assets"
        assets.mkdir()
        (assets / "server.jar").write_bytes(b"seed")
        bulk = assets / "bulk"
        bulk.mkdir()
        # Make the copying/fsync window long enough to kill deterministically.
        for index in range(2000):
            (bulk / f"file-{index}").write_bytes(b"configuration")
        first = available_range()
        allocated = set(range(first, first + 5))

        def port():
            while True:
                candidate = unused_port()
                if candidate not in allocated:
                    allocated.add(candidate)
                    return candidate

        front, control, dashboard = port(), port(), port()
        argv = ",".join(json.dumps(value) for value in [sys.executable, str(Path(__file__).resolve()),
            "--child", "{port}", "--name", "{name}"])
        source = f"""return {{
            listeners={{public='127.0.0.1:{front}'}},backends={{}},routes={{public='games'}},
            shutdown_timeout_ms=1000,instance_database='state/rift.sqlite3',
            admin={{listen='127.0.0.1:{control}',permissions={{'servers','reload','shutdown'}}}},
            web={{listen='127.0.0.1:{dashboard}',token='{SECRET}'}},
            templates={{seed={{server_jar='assets/server.jar'}},slow={{server_jar='assets/server.jar',configs='assets/bulk'}}}},
            service_groups={{
                games={{command={{{argv}}},directory='instances/{{name}}',template='seed',storage='disposable',
                    port_range={{{first},{first+4}}},autostart=true,start_timeout_ms=5000,stop_timeout_ms=1000,
                    scaling={{capacity_per_instance=10,min_instances=1,max_instances=4,cooldown_ms=500}}}},
                survival={{command={{{argv}}},directory='worlds/{{name}}',template='seed',storage='persistent',
                    port_range={{{first},{first+4}}},autostart=true,start_timeout_ms=5000,stop_timeout_ms=1000}},
                batch={{command={{{argv},'--delay-stop','10'}},directory='instances/{{name}}',template='slow',storage='disposable',
                    port_range={{{first},{first+4}}},start_timeout_ms=5000,stop_timeout_ms=15000}}
            }}
        }}"""
        config = directory / "rift.lua"
        config.write_text(source)
        database = directory / "state/rift.sqlite3"
        environment = dict(os.environ, RIFT_ADMIN_TOKEN=SECRET)

        def operation(*args):
            reply = admin(control, *args)
            assert reply["ok"], reply
            return reply["data"]

        def servers():
            return {value["name"]: value for value in operation("servers")["servers"]}

        def start(log):
            return process([str(binary), "--config", str(config)], directory, log, env=environment)

        def ready(proxy, log):
            wait_ready(proxy, lambda: "rift: admin on" in (directory / log).read_text(), directory / log, timeout=10)

        def rows():
            with sqlite3.connect(database) as connection:
                return connection.execute("SELECT name,port,phase,explicitly_stopped,definition FROM instances").fetchall()

        def failure(expected):
            result = subprocess.run([str(binary), "--config", str(config)], cwd=directory,
                env=environment, capture_output=True, text=True, timeout=10)
            assert result.returncode != 0 and expected in result.stderr, result.stderr

        with start("first.log") as proxy:
            ready(proxy, "first.log")
            eventually(lambda: "games-1" in servers() and servers()["games-1"]["state"] == "running", "scaled instance started")
            game = servers()["games-1"]
            survival = operation("create", "survival")
            eventually(lambda: servers()[survival["name"]]["state"] == "running", "survival autostart")
            operation("stop", "games-1")
            eventually(lambda: servers()["games-1"]["state"] == "stopped", "manual stop")
            (directory / "worlds/survival-1/progress").write_bytes(b"player progress")
            marker = (directory / "instances/games-1/.rift-instance.json").read_bytes()
            operation("shutdown")
            proxy.wait(timeout=8)
            assert proxy.returncode == 0

        with closing(socket.socket()) as occupied:
            occupied.bind(("127.0.0.1", survival["port"]))
            occupied.listen()
            failure(f"instances.survival-1: port 127.0.0.1:{survival['port']}")
        config.write_text(source.replace("survival={", "renamed={"))
        failure("unknown service group")
        config.write_text(source)

        with start("second.log") as proxy:
            ready(proxy, "second.log")
            eventually(lambda: servers()["survival-1"]["state"] == "running", "restored autostart")
            time.sleep(0.8)  # Allow multiple scaler ticks to expose duplicate creation.
            assert set(servers()) == {"games-1", "survival-1"}, servers()
            assert servers()["games-1"]["state"] == "stopped"
            assert servers()["games-1"]["automatic_enabled"] is False
            assert servers()["games-1"]["port"] == game["port"]
            assert servers()["survival-1"]["port"] == survival["port"]
            assert (directory / "instances/games-1/.rift-instance.json").read_bytes() == marker
            assert (directory / "worlds/survival-1/progress").read_bytes() == b"player progress"
            operation("start", "games-1")
            eventually(lambda: servers()["games-1"]["state"] == "running", "explicit restart")
            for name in ["games-1", "survival-1"]:
                operation("stop", name)
            eventually(lambda: all(value["state"] == "stopped" for value in servers().values()), "children stopped before kill")
            code, _ = api(dashboard, "POST", "/api/groups/batch/instances", wait=False)
            assert code == 202
            eventually(lambda: any(row[0] == "batch-1" and row[2] == "creating" for row in rows()), "creation journal committed")
            proxy.kill()
            proxy.wait(timeout=5)
        pending = next(row for row in rows() if row[0] == "batch-1")
        assert pending[2] == "creating", pending
        plan = json.loads(pending[4])["plan"]

        # Future batch creation can be fast; recovery uses the saved ownership.
        source = source.replace("template='slow'", "template='seed'")
        config.write_text(source)
        with start("third.log") as proxy:
            ready(proxy, "third.log")
            assert set(servers()) == {"games-1", "survival-1"}
            assert not Path(plan["directory"]).exists()
            assert not Path(plan["stage"]).exists()
            batch = operation("create", "batch")
            assert batch["name"] == "batch-2" and batch["port"] == pending[1]
            operation("start", batch["name"])
            eventually(lambda: servers()[batch["name"]]["state"] == "running", "batch started")
            child_pid = servers()[batch["name"]]["pid"]
            code, _ = api(dashboard, "DELETE", f"/api/instances/{batch['name']}", wait=False)
            assert code == 202
            eventually(lambda: (directory / "instances/batch-2/stop-requested.txt").exists(), "child accepted slow stop")
            assert next(row for row in rows() if row[0] == "batch-2")[2] == "removing"
            proxy.kill()
            proxy.wait(timeout=5)
        try:
            # The old child still owns its port: cleanup must wait for it.
            failure("still in use during removal recovery")
            assert (directory / "instances/batch-2/server.jar").exists()
        finally:
            os.kill(child_pid, signal.SIGTERM)
        eventually(lambda: port_free(batch["port"]), "orphaned child reaped")
        with start("fourth.log") as proxy:
            ready(proxy, "fourth.log")
            assert set(servers()) == {"games-1", "survival-1"}
            assert not (directory / "instances/batch-2").exists()
            assert not list((directory / "instances").glob(".rift-remove-*"))
            assert {row[0] for row in rows()} == {"games-1", "survival-1"}
            assert (directory / "worlds/survival-1/progress").read_bytes() == b"player progress"
            operation("shutdown")
            proxy.wait(timeout=8)
            assert proxy.returncode == 0
    print("PASS: SQLite restart, saved ports/ownership/stop, scaling restore, conflicts, killed creation/removal and orphan protection")


def port_free(port):
    with closing(socket.socket()) as sock:
        try:
            sock.bind(("127.0.0.1", port))
            return True
        except OSError:
            return False


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/rift")
    parser.add_argument("--child", type=int)
    parser.add_argument("--name")
    parser.add_argument("--delay-stop", type=float, default=0)
    args = parser.parse_args()
    if args.child is not None:
        child(args.child, args.name, args.delay_stop)
    else:
        if not __debug__:
            parser.error("assertions must be enabled")
        check(args.binary.resolve())
