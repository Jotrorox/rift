#!/usr/bin/env python3
"""Servers with keep_running through real proxy shutdowns, kills and restarts."""

import argparse
from contextlib import closing
import json
import os
from pathlib import Path
import sqlite3
import sys
import tempfile

from instance_persistence_wire import port_free
from managed_wire import SECRET, admin, eventually
from minecraft import ROOT, process, unused_port, wait_ready
from services_wire import available_range

SERVER = Path(__file__).with_name("instance_persistence_wire.py").resolve()


def check(binary):
    with tempfile.TemporaryDirectory(prefix="rift-kept-servers-") as temporary:
        directory = Path(temporary).resolve()
        assets = directory / "assets"
        assets.mkdir()
        (assets / "server.jar").write_bytes(b"seed")
        first = available_range()
        allocated = set(range(first, first + 5))

        def port():
            while True:
                candidate = unused_port()
                if candidate not in allocated:
                    allocated.add(candidate)
                    return candidate

        front, control = port(), port()
        argv = ",".join(json.dumps(value) for value in [sys.executable, str(SERVER),
            "--child", "{port}", "--name", "{name}"])
        config = directory / "rift.lua"
        config.write_text(f"""return {{
            listeners={{public='127.0.0.1:{front}'}},backends={{}},routes={{public='kept'}},
            shutdown_timeout_ms=1000,instance_database='state/rift.sqlite3',
            admin={{listen='127.0.0.1:{control}',permissions={{'servers','shutdown'}}}},
            templates={{seed={{server_jar='assets/server.jar'}}}},
            service_groups={{
                kept={{command={{{argv}}},directory='instances/{{name}}',template='seed',storage='disposable',
                    port_range={{{first},{first+4}}},autostart=true,start_timeout_ms=5000,stop_timeout_ms=5000,
                    keep_running=true}},
                plain={{command={{{argv}}},directory='worlds/{{name}}',template='seed',storage='persistent',
                    port_range={{{first},{first+4}}},autostart=true,start_timeout_ms=5000,stop_timeout_ms=5000}}
            }}
        }}""")
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

        def update(statement):
            with closing(sqlite3.connect(database)) as connection:
                connection.execute(statement)
                connection.commit()

        def running(name, pid=None):
            server = servers().get(name)
            return server and server["state"] == "running" and (pid is None or server["pid"] == pid)

        def shutdown(proxy, *arguments):
            operation("shutdown", *arguments)
            proxy.wait(timeout=10)
            assert proxy.returncode == 0

        with start("first.log") as proxy:
            ready(proxy, "first.log")
            kept, plain = operation("create", "kept"), operation("create", "plain")
            eventually(lambda: running("kept-1") and running("plain-1"), "instances started")
            pid = servers()["kept-1"]["pid"]
            record = json.loads((directory / "instances/kept-1/rift-process.json").read_text())
            assert record["pid"] == pid, record
            assert not (directory / "worlds/plain-1/rift-process.json").exists()
            shutdown(proxy)
        # An orderly shutdown stops only the server without the setting.
        assert port_free(plain["port"]) and not port_free(kept["port"])

        with start("second.log") as proxy:
            ready(proxy, "second.log")
            eventually(lambda: running("kept-1", pid), "kept server adopted with its pid")
            eventually(lambda: running("plain-1"), "plain server started again")
            assert (directory / "instances/kept-1/server.pid").read_text() == str(pid)
            operation("stop", "plain-1")
            eventually(lambda: servers()["plain-1"]["state"] == "stopped", "plain server stopped")
            proxy.kill()
            proxy.wait(timeout=5)
        # A killed proxy leaves it running as well.
        assert not port_free(kept["port"])

        # A stop the database knows about wins over the running process.
        update("UPDATE instances SET explicitly_stopped=1 WHERE name='kept-1'")
        with start("third.log") as proxy:
            ready(proxy, "third.log")
            eventually(lambda: port_free(kept["port"]), "adopted server stopped")
            assert (directory / "instances/kept-1/stop-requested.txt").exists()
            server = servers()["kept-1"]
            assert server["state"] == "stopped" and server["automatic_enabled"] is False, server
            assert not (directory / "instances/kept-1/rift-process.json").exists()
            operation("start", "kept-1")
            eventually(lambda: running("kept-1"), "explicit restart")
            assert servers()["kept-1"]["pid"] != pid
            proxy.kill()
            proxy.wait(timeout=5)
        assert not port_free(kept["port"])

        # A removal the killed proxy had begun stops the server, then deletes it.
        update("UPDATE instances SET phase='removing' WHERE name='kept-1'")
        with start("fourth.log") as proxy:
            ready(proxy, "fourth.log")
            assert "kept-1" not in servers()
            assert port_free(kept["port"])
            assert not (directory / "instances/kept-1").exists()
            again = operation("create", "kept")
            eventually(lambda: running(again["name"]), "new kept instance started")
            shutdown(proxy, "all")
        assert port_free(again["port"])
        assert (directory / "instances" / again["name"] / "stop-requested.txt").exists()
    print("PASS: kept servers survive shutdown and kill, adoption, saved stop, removal recovery and shutdown all")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/rift")
    args = parser.parse_args()
    if not __debug__:
        parser.error("assertions must be enabled")
    if sys.platform not in ("linux", "darwin"):
        print("SKIP: keep_running requires Linux or macOS")
    else:
        check(args.binary.resolve())
