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


def check(binary, server, template_storage=None):
    if template_storage and server == "pumpkin":
        raise ValueError("template provisioning requires a Java server jar; Pumpkin is unsupported")
    runs = CACHE / "runs"
    runs.mkdir(parents=True, exist_ok=True)
    suffix = f"{template_storage}-" if template_storage else ""
    directory = Path(tempfile.mkdtemp(prefix=f"managed-{server}-{suffix}", dir=runs))
    backend = directory / "server"
    backend.mkdir()
    ports = set()
    while len(ports) < 3:
        ports.add(unused_port())
    front, control, destination = sorted(ports)
    command = configure_server(server, backend, destination, True)
    instance = "game-1" if template_storage else "game"
    if template_storage:
        seed_map = directory / "seed-map"
        seed_map.mkdir()
        seed_contents = "rift template seed map\n"
        (seed_map / "rift-map-sentinel.txt").write_text(seed_contents)
        jar = str(Path(command[-2]).resolve())
        command[-2] = "server.jar"
        backend = directory / "instances" / instance
        definitions = f"""backends={{}},
          templates={{game={{server_jar={json.dumps(jar)},configs='server',map='seed-map'}}}},
          service_groups={{game={{template='game',storage='{template_storage}',
            command={{{','.join(json.dumps(argument) for argument in command)}}},
            directory='instances/{{name}}',port_range={{{destination},{destination}}},
            idle_timeout_ms=1000,start_timeout_ms=120000,stop_timeout_ms=30000}}}}"""
    else:
        definitions = f"""backends={{game='127.0.0.1:{destination}'}},
          managed_servers={{game={{command={{{','.join(json.dumps(argument) for argument in command)}}},
            directory='server',idle_timeout_ms=1000,start_timeout_ms=120000,stop_timeout_ms=30000}}}}"""
    config = directory / "rift.lua"
    config.write_text(f"""return {{
      listeners={{public='127.0.0.1:{front}'}},
      {definitions},routes={{public='game'}},
      {"network={hubs={'game'}}," if server != "pumpkin" else ""}
      limits={{connect_timeout_ms=10000}},shutdown_timeout_ms=1000,
      admin={{listen='127.0.0.1:{control}',permissions={{'servers','shutdown'}}}},
    }}""")
    environment = dict(os.environ, RIFT_ADMIN_TOKEN=SECRET)
    log = directory / "proxy.log"
    with process([str(binary), "--config", str(config)], directory, log.name, env=environment) as proxy:
        wait_ready(proxy, lambda: "rift: admin on" in log.read_text(), log, timeout=10)
        if template_storage:
            created = admin(control, "create", "game")
            assert created["ok"], created
            assert created["data"]["name"] == instance, created
            assert created["data"]["port"] == destination, created
            assert created["data"]["storage"] == template_storage, created
            properties = (backend / "server.properties").read_text()
            assert f"server-port={destination}\n" in properties, properties
            assert "server-ip=127.0.0.1\n" in properties, properties
            assert "level-name=world\n" in properties, properties
            marker = json.loads((backend / ".rift-instance.json").read_text())
            assert marker["name"] == instance and marker["group"] == "game", marker
            assert marker["template"] == "game" and marker["storage"] == template_storage, marker
            map_sentinel = backend / "world" / "rift-map-sentinel.txt"
            assert map_sentinel.read_text() == seed_contents
            assert (backend / "server.jar").is_file()

        def state():
            result = admin(control, "servers")
            assert result["ok"], result
            return next(server for server in result["data"]["servers"] if server["name"] == instance)

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
                assert not admin(control, "stop", instance)["ok"]
                if template_storage:
                    assert map_sentinel.read_text() == seed_contents
            eventually(lambda: state()["state"] == "stopped", "world saved and server stopped", timeout=40)
            if template_storage:
                assert map_sentinel.read_text() == seed_contents
                assert (backend / "world" / "level.dat").is_file(), "real server did not save its world"
        assert pids[0] != pids[1], pids
        assert (backend / "world").exists()
        if template_storage:
            removed = admin(control, "remove", instance)
            assert removed["ok"], removed
            assert removed["data"]["removed"] is True, removed
            assert removed["data"]["files_removed"] is (template_storage == "disposable"), removed
            assert not removed["data"].get("cleanup_error"), removed
            listing = admin(control, "servers")
            assert listing["ok"] and not listing["data"]["servers"], listing
            if template_storage == "persistent":
                assert map_sentinel.read_text() == seed_contents
                assert (backend / "world" / "level.dat").is_file()
                assert (backend / ".rift-instance.json").is_file()
            else:
                assert not backend.exists(), "disposable removal retained instance files"
            assert (directory / "seed-map" / "rift-map-sentinel.txt").read_text() == seed_contents
            assert (directory / "server" / "eula.txt").read_text() == "eula=true\n"
        assert admin(control, "shutdown")["ok"]
        proxy.wait(timeout=10)
        assert proxy.returncode == 0
    result = {"server": server, "passed": True, "pids": pids, "artifacts": str(directory)}
    if template_storage:
        result.update(template_storage=template_storage, files_removed=template_storage == "disposable")
    (directory / "result.json").write_text(json.dumps(result, indent=2))
    print(json.dumps(result))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/rift")
    parser.add_argument("--server", choices=("vanilla", "paper", "pumpkin"), default="paper")
    parser.add_argument("--accept-eula", action="store_true")
    parser.add_argument("--template-storage", choices=("persistent", "disposable"),
                        help="provision a Java server from template assets and verify removal storage policy")
    args = parser.parse_args()
    if not args.accept_eula or not __debug__:
        parser.error("--accept-eula and enabled assertions are required")
    if args.template_storage and args.server == "pumpkin":
        parser.error("--template-storage requires a Java server jar and cannot be used with Pumpkin")
    check(args.binary.resolve(), args.server, args.template_storage)
