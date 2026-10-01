#!/usr/bin/env python3
"""Managed processes through real Minecraft wire sessions, without downloads."""

import argparse
from concurrent.futures import ThreadPoolExecutor
from contextlib import closing
import json
import os
from pathlib import Path
import socket
import struct
import sys
import tempfile
from threading import Barrier
import time
from urllib.error import HTTPError
from urllib.request import Request, urlopen

from minecraft import ROOT, process, status, unused_port, wait_ready
from network_wire import Backend, NetworkClient

SECRET = "rift-managed-wire-test-secret-32-bytes"


def eventually(predicate, description, timeout=10):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(0.05)
    raise AssertionError(f"timed out: {description}")


def child(port, delay):
    with Path("starts.txt").open("a") as output:
        output.write(f"{os.getpid()}\n")
    time.sleep(delay)
    backend = Backend("sleepy", 42, port=port)
    try:
        for line in sys.stdin:
            if line.strip() == "stop":
                Path("stopped.txt").write_text(str(os.getpid()))
                break
    finally:
        backend.close()


def admin(port, *args):
    with socket.create_connection(("127.0.0.1", port), timeout=5) as connection:
        connection.sendall((json.dumps({"token": SECRET, "args": args}) + "\n").encode())
        with connection.makefile("rb") as reader:
            return json.loads(reader.readline())


def web(port, operation):
    request = Request(f"http://127.0.0.1:{port}/api/servers/sleepy/{operation}",
                      data=b"{}", headers={"Authorization": f"Bearer {SECRET}",
                                           "Content-Type": "application/json"})
    try:
        with urlopen(request, timeout=5) as response:
            return response.status, json.load(response)
    except HTTPError as response:
        return response.code, json.load(response)


def check(binary):
    with tempfile.TemporaryDirectory(prefix="rift-managed-wire-") as temporary, closing(Backend("lobby", 7)) as lobby:
        directory = Path(temporary)
        server_directory = directory / "server"
        server_directory.mkdir()
        ports = set()
        while len(ports) < 4:
            ports.add(unused_port())
        front, control, backend_port, dashboard = sorted(ports)
        command = [sys.executable, str(Path(__file__).resolve()), "--child", str(backend_port), "--delay", "1.5"]
        argv = ",".join(json.dumps(arg) for arg in command)
        source = f"""return {{
          listeners={{public='127.0.0.1:{front}'}},
          backends={{sleepy='127.0.0.1:{backend_port}',lobby='127.0.0.1:{lobby.port}'}}, routes={{public='sleepy'}},
          network={{hubs={{'lobby'}}}},
          limits={{connect_timeout_ms=500}}, shutdown_timeout_ms=1000,
          health_check={{interval_ms=100,timeout_ms=50,unhealthy_threshold=1}},
          status_cache={{ttl_ms=100}},
          admin={{listen='127.0.0.1:{control}', permissions={{'status','servers','reload','shutdown','drain'}}}},
          web={{listen='127.0.0.1:{dashboard}',token='{SECRET}'}},
          managed_servers={{sleepy={{command={{{argv}}},directory='server',
            start_timeout_ms=5000,stop_timeout_ms=1500,idle_timeout_ms=500,restart_delay_ms=100}}}}
        }}"""
        config = directory / "rift.lua"
        config.write_text(source)
        environment = dict(os.environ, RIFT_ADMIN_TOKEN=SECRET)
        log = directory / "proxy.log"
        with process([str(binary), "--config", str(config)], directory, log.name, env=environment) as proxy:
            try:
                wait_ready(proxy, lambda: "rift: admin on" in log.read_text(), log, timeout=10)

                def state():
                    response = admin(control, "servers")
                    assert response["ok"], response
                    return response["data"]["servers"][0]

                def starts():
                    path = server_directory / "starts.txt"
                    return path.read_text().splitlines() if path.exists() else []

                # Probes and server-list refreshes must not spend resources.
                for _ in range(3):
                    status(front)
                assert state()["state"] == "stopped", state()
                assert starts() == []

                # Access/drain checks must precede process creation.
                assert admin(control, "drain", "sleepy", "on")["ok"]
                with NetworkClient(front, "Drained") as client:
                    client.until("disconnect")
                assert starts() == []
                assert admin(control, "drain", "sleepy", "off")["ok"]

                def join(name):
                    client = NetworkClient(front, name)
                    try:
                        client.joined("sleepy")
                        client.probe("sleepy")
                        return client
                    except BaseException:
                        client.__exit__()
                        raise

                barrier = Barrier(75)

                def concurrent_join(index):
                    barrier.wait(timeout=10)
                    return join(f"Burst{index:02}")

                with ThreadPoolExecutor(max_workers=75) as pool:
                    clients = list(pool.map(concurrent_join, range(75)))
                try:
                    assert len(clients) == 75
                    assert len(starts()) == 1, starts()
                    assert state()["players"] == 75, state()
                    current_pid = state()["pid"]
                    time.sleep(0.8)  # longer than idle timeout with active players
                    assert state()["state"] == "running", state()
                    assert not admin(control, "stop", "sleepy")["ok"]
                    assert web(dashboard, "stop")[0] == 409
                    assert state()["pid"] == current_pid
                    assert admin(control, "reload")["ok"]
                    assert state()["pid"] == current_pid, "reload restarted owned process"
                    config.write_text(source.replace("idle_timeout_ms=500", "idle_timeout_ms=600"))
                    assert not admin(control, "reload")["ok"], "managed changes accepted on reload"
                    config.write_text(source)
                    for client in clients:
                        client.probe("sleepy")
                finally:
                    for client in clients:
                        client.__exit__()
                eventually(lambda: state()["state"] == "stopped", "idle graceful stop")
                assert (server_directory / "stopped.txt").read_text() == str(current_pid)

                # A new login wakes the same persistent directory again.
                with join("Carol") as client:
                    assert len(starts()) == 2
                    assert state()["pid"] != current_pid
                    client.command("server lobby")
                    client.joined("lobby")
                    client.probe("lobby")
                    eventually(lambda: state()["state"] == "stopped", "transferred server idles down")
                    record = next(record for record in lobby.connections if record["name"] == "Carol")

                    def keepalive_during_start():
                        time.sleep(0.15)
                        payload = struct.pack(">q", 424242)
                        record["peer"].send(client.packets["keepalive"], payload)
                        expected = (client.packets["keepalive_reply"], payload)
                        eventually(lambda: expected in record["packets"],
                                   "old backend keepalive forwarded during cold startup", timeout=0.9)

                    with ThreadPoolExecutor(max_workers=1) as pool:
                        check_keepalive = pool.submit(keepalive_during_start)
                        client.command("server sleepy")
                        client.joined("sleepy")
                        check_keepalive.result()
                    client.probe("sleepy")
                    assert len(starts()) == 3, "transfer must wake sleeping target"
                eventually(lambda: state()["players"] == 0 and state()["reservations"] == 0,
                           "disconnect releases attachment")
                assert admin(control, "stop", "sleepy")["ok"]
                eventually(lambda: state()["state"] == "stopped", "explicit stop")
                with NetworkClient(front, "Disabled") as client:
                    client.until("disconnect")
                assert len(starts()) == 3, "manual stop must disable automatic wake"
                assert web(dashboard, "start")[0] == 202
                eventually(lambda: state()["state"] == "running", "explicit start")
                assert len(starts()) == 4
                final_pid = state()["pid"]
                assert admin(control, "shutdown")["ok"]
                proxy.wait(timeout=8)
                assert proxy.returncode == 0
                assert (server_directory / "stopped.txt").read_text() == str(final_pid)
                with socket.socket() as probe:
                    assert probe.connect_ex(("127.0.0.1", backend_port)) != 0
            except BaseException:
                print(log.read_text(), flush=True)
                server_log = server_directory / "rift-server.log"
                if server_log.exists():
                    print(server_log.read_text(), flush=True)
                raise
    print("PASS: managed cold login, concurrent wake, idle/occupied stop, reload, manual control, shutdown cleanup")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/rift")
    parser.add_argument("--child", type=int)
    parser.add_argument("--delay", type=float, default=0)
    args = parser.parse_args()
    if not __debug__:
        parser.error("assertions must be enabled")
    if args.child:
        child(args.child, args.delay)
    else:
        check(args.binary.resolve())
