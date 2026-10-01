#!/usr/bin/env python3
"""Cold start, world delivery and graceful idle shutdown of a real managed server."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import tempfile
import uuid

from managed_wire import SECRET, admin, eventually
from minecraft import CACHE, ROOT, SERVERS, configure_server, process, status, unused_port, wait_ready
from network_minecraft import RealClient
from network_wire import NetworkClient


class PumpkinClient(NetworkClient):
    def __init__(self, port, name, protocol):
        super().__init__(port, name, chat=False, protocol=protocol)

    def player_identity(self):
        return uuid.UUID(bytes=hashlib.sha256(self.name.encode()).digest()[:16])

    def ready(self, generation, previous_chunks, previous_positions):
        chunks = positions = 0
        for _ in range(10000):
            event, value = self.next_packet()
            assert event != "disconnect", value
            if event == "packet":
                packet_id, _ = value
                chunks += packet_id == self.packets["chunk"]
                positions += packet_id == self.packets["position"]
            if len(self.joins) >= generation and chunks > previous_chunks and positions > previous_positions:
                return
        raise AssertionError("backend did not deliver a world, teleport and chunks")


def check(binary, server):
    runs = CACHE / "runs"
    runs.mkdir(parents=True, exist_ok=True)
    directory = Path(tempfile.mkdtemp(prefix=f"managed-{server}-", dir=runs))
    backend = directory / "server"
    backend.mkdir()
    ports = set()
    while len(ports) < 3:
        ports.add(unused_port())
    front, control, destination = sorted(ports)
    command = configure_server(server, backend, destination, True)
    config = directory / "rift.lua"
    config.write_text(f"""return {{
      listeners={{public='127.0.0.1:{front}'}},
      backends={{game='127.0.0.1:{destination}'}},routes={{public='game'}},
      {"network={hubs={'game'}}," if server != "pumpkin" else ""}
      limits={{connect_timeout_ms=10000}},shutdown_timeout_ms=1000,
      admin={{listen='127.0.0.1:{control}',permissions={{'servers','shutdown'}}}},
      managed_servers={{game={{command={{{','.join(json.dumps(argument) for argument in command)}}},
        directory='server',idle_timeout_ms=1000,start_timeout_ms=120000,stop_timeout_ms=30000}}}}
    }}""")
    environment = dict(os.environ, RIFT_ADMIN_TOKEN=SECRET)
    log = directory / "proxy.log"
    with process([str(binary), "--config", str(config)], directory, log.name, env=environment) as proxy:
        wait_ready(proxy, lambda: "rift: admin on" in log.read_text(), log, timeout=10)

        def state():
            result = admin(control, "servers")
            assert result["ok"], result
            return result["data"]["servers"][0]

        assert state()["state"] == "stopped"
        status(front, SERVERS[server]["protocol"])
        assert state()["state"] == "stopped"
        pids = []
        for name in ("ManagedOne", "ManagedTwo"):
            client_type = PumpkinClient if server == "pumpkin" else RealClient
            with client_type(front, name, SERVERS[server]["protocol"]) as client:
                client.read_timeout = 120
                client.ready(1, 0, 0)
                pids.append(state()["pid"])
                assert state()["state"] == "running"
                assert not admin(control, "stop", "game")["ok"]
            eventually(lambda: state()["state"] == "stopped", "world saved and server stopped", timeout=40)
        assert pids[0] != pids[1], pids
        assert (backend / "world").exists()
        assert admin(control, "shutdown")["ok"]
        proxy.wait(timeout=10)
        assert proxy.returncode == 0
    result = {"server": server, "passed": True, "pids": pids, "artifacts": str(directory)}
    (directory / "result.json").write_text(json.dumps(result, indent=2))
    print(json.dumps(result))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/rift")
    parser.add_argument("--server", choices=("vanilla", "paper", "pumpkin"), default="paper")
    parser.add_argument("--accept-eula", action="store_true")
    args = parser.parse_args()
    if not args.accept_eula or not __debug__:
        parser.error("--accept-eula and enabled assertions are required")
    check(args.binary.resolve(), args.server)
