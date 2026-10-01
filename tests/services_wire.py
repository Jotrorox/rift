#!/usr/bin/env python3
"""Dynamic service groups through real Minecraft sessions, without downloads."""

import argparse
from concurrent.futures import ThreadPoolExecutor
from contextlib import ExitStack, closing
import json
import os
import signal
from pathlib import Path
import socket
import sys
import tempfile
from urllib.error import HTTPError
from urllib.request import Request, urlopen

from managed_wire import SECRET, admin, eventually
from minecraft import ROOT, process, status, unused_port, wait_ready
from network_wire import Backend, NetworkClient


def child(port, name):
    Path("started.txt").write_text(str(os.getpid()))
    with closing(Backend(name, port, port=port)):
        for line in sys.stdin:
            if line.strip() == "stop":
                Path("stopped.txt").write_text(str(os.getpid()))
                break


def api(port, method, path):
    request = Request(f"http://127.0.0.1:{port}{path}", method=method,
                      data=None if method == "GET" else b"{}",
                      headers={"Authorization": f"Bearer {SECRET}",
                               "Content-Type": "application/json"})
    try:
        with urlopen(request, timeout=10) as response:
            return response.status, json.load(response)
    except HTTPError as response:
        return response.code, json.load(response)


def available_range():
    for _ in range(100):
        first = unused_port()
        if first > 65530:
            continue
        with ExitStack() as stack:
            try:
                for port in range(first, first + 5):
                    listener = stack.enter_context(socket.socket())
                    listener.bind(("127.0.0.1", port))
                return first
            except OSError:
                continue
    raise AssertionError("unable to find consecutive fixture ports")


def check(binary):
    with tempfile.TemporaryDirectory(prefix="rift-services-wire-") as temporary:
        directory = Path(temporary)
        first = available_range()
        ports = set(range(first, first + 5))
        def allocate():
            while True:
                value = unused_port()
                if value not in ports:
                    ports.add(value)
                    return value
        front, control, dashboard = allocate(), allocate(), allocate()
        command = [sys.executable, str(Path(__file__).resolve()),
                   "--child", "{port}", "--name", "{name}"]
        argv = ",".join(json.dumps(argument) for argument in command)
        source = f"""return {{
          listeners={{public='127.0.0.1:{front}'}}, backends={{}}, routes={{public='lobby'}},
          network={{hubs={{'lobby'}}}}, shutdown_timeout_ms=1000,
          admin={{listen='127.0.0.1:{control}', permissions={{'status','servers','reload','shutdown','drain'}}}},
          web={{listen='127.0.0.1:{dashboard}',token='{SECRET}'}},
          service_groups={{lobby={{command={{{argv}}},directory='servers/{{name}}',
            port_range={{{first},{first+4}}}, start_timeout_ms=5000,stop_timeout_ms=1000}}}}
        }}"""
        config = directory / "rift.lua"
        config.write_text(source)
        log = directory / "proxy.log"
        environment = dict(os.environ, RIFT_ADMIN_TOKEN=SECRET)
        with closing(socket.socket()) as occupied:
            occupied.bind(("127.0.0.1", first))
            occupied.listen()
            with process([str(binary), "--config", str(config)], directory, log.name, env=environment) as proxy:
                try:
                    wait_ready(proxy, lambda: "rift: admin on" in log.read_text(), log, timeout=10)
                    proxy_pid = proxy.pid
                    def operation(*args):
                        response = admin(control, *args)
                        assert response["ok"], response
                        return response["data"]
                    def servers():
                        return {server["name"]: server for server in operation("servers")["servers"]}
                    assert operation("groups")["groups"][0]["instances"] == []
                    status(front)  # An empty group must still answer proxy pings.
                    assert servers() == {}
                    with ThreadPoolExecutor(max_workers=2) as pool:
                        instances = list(pool.map(lambda _: operation("create", "lobby"), range(2)))
                    assert {value["name"] for value in instances} == {"lobby-1", "lobby-2"}
                    assert {value["port"] for value in instances} == {first+1, first+2}
                    assert all(server["state"] == "stopped" for server in servers().values())
                    with NetworkClient(front, "Alice") as alice:
                        alice.joined("lobby-1")
                        alice.probe("lobby-1")
                        with NetworkClient(front, "Bob") as bob:
                            bob.joined("lobby-2")
                            bob.probe("lobby-2")
                            assert not admin(control, "remove", "lobby-2")["ok"]
                            assert api(dashboard, "DELETE", "/api/instances/lobby-1")[0] == 409
                            code, third = api(dashboard, "POST", "/api/groups/lobby/instances")
                            assert code == 201, third
                            assert third["name"] == "lobby-3" and third["port"] == first+3
                            # The connection was accepted before this instance existed.
                            alice.command("server lobby-3")
                            alice.joined("lobby-3")
                            alice.probe("lobby-3")
                            pid = servers()["lobby-3"]["pid"]
                            operation("reload")
                            assert servers()["lobby-3"]["pid"] == pid
                            assert proxy.pid == proxy_pid
                            config.write_text(source.replace("stop_timeout_ms=1000", "stop_timeout_ms=1200"))
                            assert not admin(control, "reload")["ok"]
                            config.write_text(source)
                            bob.probe("lobby-2")
                            alice.probe("lobby-3")
                        eventually(lambda: servers()["lobby-2"]["reservations"] == 0,
                                   "Bob's attachment releases")
                        old_pid = servers()["lobby-2"]["pid"]
                        operation("remove", "lobby-2")
                        assert "lobby-2" not in servers()
                        assert (directory / "servers/lobby-2/stopped.txt").read_text() == str(old_pid)
                        fourth = operation("create", "lobby")
                        assert fourth["name"] == "lobby-4" and fourth["port"] == first+2
                        fifth = operation("create", "lobby")
                        assert fifth["name"] == "lobby-5" and fifth["port"] == first+4
                        assert not admin(control, "create", "lobby")["ok"]
                        operation("drain", "lobby-1", "on")
                        alice.command("server lobby")
                        alice.joined("lobby-4")
                        alice.probe("lobby-4")
                        assert api(dashboard, "DELETE", "/api/instances/lobby-3")[0] == 200
                        assert "lobby-3" not in servers()
                        assert api(dashboard, "GET", "/api/groups")[1]["groups"][0]["instances"] == ["lobby-1", "lobby-4", "lobby-5"]
                        # Outage recovery must discover a hub registered after
                        # this player logged in and honor the drain on lobby-1.
                        os.kill(servers()["lobby-4"]["pid"], signal.SIGTERM)
                        alice.joined("lobby-5")
                        alice.probe("lobby-5")
                    eventually(lambda: all(server["reservations"] == 0 for server in servers().values()),
                               "all attachments release")
                    for name in list(servers()):
                        operation("remove", name)
                    assert servers() == {}
                    status(front)
                    operation("shutdown")
                    proxy.wait(timeout=8)
                    assert proxy.returncode == 0
                    for port in range(first+1, first+5):
                        with socket.socket() as probe:
                            assert probe.connect_ex(("127.0.0.1", port)) != 0
                except BaseException:
                    print(log.read_text(), flush=True)
                    for server_log in directory.glob("servers/*/rift-server.log"):
                        print(f"{server_log}:\n{server_log.read_text()}", flush=True)
                    raise
    print("PASS: dynamic groups, concurrent allocation, balancing, live transfer, crash recovery, reload, occupied removal, port reuse, exhaustion, cleanup")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/rift")
    parser.add_argument("--child", type=int)
    parser.add_argument("--name")
    args = parser.parse_args()
    if not __debug__:
        parser.error("assertions must be enabled")
    if args.child:
        child(args.child, args.name)
    else:
        check(args.binary.resolve())
