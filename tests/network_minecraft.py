#!/usr/bin/env python3
"""Run the two-server acceptance scenario against every pinned switchable vanilla version.

Uses one compressed frontend socket throughout lobby -> survival -> lobby,
then kills survival and checks recovery to lobby. Also verifies actual backend
bans and unsigned chat after every transition. Requires the fixture-specific Java runtime
and explicit --accept-eula. Artifacts/logs are retained under target/minecraft/runs/.
"""

import argparse
import io
from contextlib import ExitStack
from pathlib import Path
import struct
import tempfile
import time
import uuid

from minecraft import (CACHE, ROOT, SERVERS, configure_server, empty_chat_update, process,
                       read_varint, status_ready, string, unused_port, wait_ready)
from network_wire import NETWORK_PROTOCOLS, NetworkClient, read_string

SWITCHABLE_SERVERS = tuple(name for name, fixture in SERVERS.items() if fixture.get("switchable"))


class RealClient(NetworkClient):
    def __init__(self, port, name, protocol):
        super().__init__(port, name, chat=False, protocol=protocol)
        self.command_trees = 0
        self.chunks = self.positions = self.keepalives = 0
        self.respawns = 0
        self.gamemodes, self.borders = [], []

    def next_packet(self):
        event, value = super().next_packet()
        if event == "packet" and self.phase == "play":
            packet_id, body = value
            if packet_id in (self.packets["chunk"], self.packets.get("chunk_bulk", -1)):
                self.chunks += 1
            elif packet_id == self.packets["position"]:
                self.positions += 1
            elif packet_id == self.packets["keepalive"]:
                self.keepalives += 1
            elif packet_id == self.packets.get("respawn"):
                self.respawns += 1
            elif packet_id == self.packets["border"]:
                data = io.BytesIO(body)
                if self.protocol < 755:
                    action = read_varint(data)
                    if action == 0:
                        self.borders.append(struct.unpack(">d", data.read(8))[0])
                        return event, value
                    if action != 3:
                        return event, value
                self.borders.append(struct.unpack(">dddd", data.read(32))[3])
            elif packet_id == self.packets["commands"]:
                assert string("server") in body and string("hub") in body
                self.command_trees += 1
            elif packet_id == self.packets["join"]:
                data = io.BytesIO(body)
                if self.protocol < 764:
                    data.read(5 if self.protocol >= 751 else 4)
                    self.gamemodes.append(data.read(1)[0] & 7)
                else:
                    data.read(5)
                    for _ in range(read_varint(data)):
                        read_string(data)
                    for _ in range(3):
                        read_varint(data)
                    data.read(3)
                    if self.protocol >= 766:
                        read_varint(data)
                    else:
                        read_string(data)
                    assert read_string(data) == "minecraft:overworld"
                    data.read(8)
                    self.gamemodes.append(data.read(1)[0])
        return event, value

    def ready(self, generation, previous_chunks, previous_positions):
        for _ in range(10000):
            if (len(self.joins) >= generation and self.chunks > previous_chunks
                    and self.positions > previous_positions
                    and (self.protocol >= 764 or self.respawns >= generation - 1)
                    and (self.protocol < 393 or self.command_trees >= generation)):
                return
            event, body = self.next_packet()
            assert event != "disconnect", body
        raise AssertionError("backend did not deliver a new world, teleport and chunks")

    def chat_message(self, value):
        # Offline fixtures intentionally use unsigned messages. The session's
        # signed-key/chain boundary is independently exercised by network_wire.
        data = string(value)
        if self.protocol >= 759:
            data += struct.pack(">qq", int(time.time() * 1000), 0) + empty_chat_update(self.protocol)
        self.send(self.packets["chat"], data)


def console(server, command):
    server.stdin.write(command + "\n")
    server.stdin.flush()


def verify_backend(client, server, logfile, label):
    marker = "RiftNetwork" + uuid.uuid4().hex[:8]
    console(server, f'tellraw {client.name} {{"text":"{label}-{marker}"}}')
    client.until("message", f"{label}-{marker}".encode())
    is_lobby = label.startswith("lobby")
    assert client.gamemodes[-1] == (1 if is_lobby else 0), client.gamemodes
    assert client.borders[-1] == (128.0 if is_lobby else 256.0), client.borders
    chat_marker = "RiftChat" + uuid.uuid4().hex[:8]
    client.chat_message(chat_marker)
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        if chat_marker in logfile.read_text():
            break
        time.sleep(0.05)
    else:
        raise AssertionError(f"{label}: backend did not accept chat after joining")


def check(binary, fixture_name):
    fixture = SERVERS[fixture_name]
    protocol = fixture["protocol"]
    assert protocol in NETWORK_PROTOCOLS, f"missing network client for {fixture_name}"
    label = fixture["version"]
    runs = CACHE / "runs"
    runs.mkdir(parents=True, exist_ok=True)
    directory = Path(tempfile.mkdtemp(prefix=f"network-{fixture_name}-", dir=runs))
    print(f"Network acceptance logs: {directory}", flush=True)
    ports = {}
    while len(set(ports.values())) < 3:
        ports = {name: unused_port() for name in ("lobby", "survival", "proxy")}
    directories = {name: directory / name for name in ("lobby", "survival")}
    with ExitStack() as stack:
        servers = {}
        for name in ("lobby", "survival"):
            directories[name].mkdir()
            command = configure_server(fixture_name, directories[name], ports[name], name == "lobby",
                                       motd=f"rift-network-{name}")
            if name == "survival":
                properties = directories[name] / "server.properties"
                properties.write_text(properties.read_text().replace("gamemode=creative", "gamemode=survival")
                                      .replace("gamemode=1", "gamemode=0"))
            servers[name] = stack.enter_context(process(command, directories[name], "server.log", server=True))
        for name, server in servers.items():
            wait_ready(server, lambda name=name: (
                "Done (" in (directories[name] / "server.log").read_text()
                and status_ready(ports[name], protocol)),
                       directories[name] / "server.log")
            console(server, "worldborder set " + ("128" if name == "lobby" else "256"))
        config = directory / "network.lua"
        config.write_text(f"""return {{
          listeners = {{ public = '127.0.0.1:{ports['proxy']}' }},
          backends = {{ lobby = '127.0.0.1:{ports['lobby']}', survival = '127.0.0.1:{ports['survival']}' }},
          routes = {{ public = 'lobby' }},
          network = {{ initial = {{'lobby'}}, hubs = {{'lobby'}} }},
        }}""")
        proxy = stack.enter_context(process([str(binary), "--config", str(config)], directory, "proxy.log"))
        wait_ready(proxy, lambda: "rift:" in (directory / "proxy.log").read_text(), directory / "proxy.log")
        name = "RiftNet" + uuid.uuid4().hex[:8]
        with RealClient(ports["proxy"], name, protocol) as client:
            client.deadline = time.monotonic() + 240
            client.ready(1, 0, 0)
            verify_backend(client, servers["lobby"], directories["lobby"] / "server.log", "lobby-initial")
            with RealClient(ports["proxy"], name, protocol) as duplicate:
                assert b"already connected" in duplicate.until("disconnect")
            verify_backend(client, servers["lobby"], directories["lobby"] / "server.log", "lobby-after-duplicate")
            assert f"{name} lost connection" not in (directories["lobby"] / "server.log").read_text()
            for generation, (command, target) in enumerate(
                    [("server survival", "survival"), ("hub", "lobby")], start=2):
                chunks, positions = client.chunks, client.positions
                client.command(command, signed=command == "hub")
                client.ready(generation, chunks, positions)
                verify_backend(client, servers[target], directories[target] / "server.log", target)
            assert client.transitions == 2
            assert client.threshold == 256, "frontend compression changed with backend"
            assert client.compressed_packets and client.sent_compressed_packets
            print(f"PASS real vanilla {label}: one socket, lobby -> survival -> lobby, fresh worlds/chunks, chat", flush=True)
            # Target login ban must keep the player connected to the old lobby.
            console(servers["survival"], f"ban {name} network acceptance ban")
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline and "network acceptance ban" not in (directories["survival"] / "banned-players.json").read_text():
                time.sleep(0.05)
            before = client.transitions
            client.command("server survival")
            reason = client.until("message", b"ban")
            if protocol < 765:
                message = io.BytesIO(reason)
                read_string(message)
                assert read_varint(message) == (1 if protocol <= 759 else 0)
                if 735 <= protocol < 759:
                    assert message.read(16) == bytes(16)
                assert not message.read(), "trailing proxy system-message fields"
            verify_backend(client, servers["lobby"], directories["lobby"] / "server.log", "lobby-after-ban")
            assert client.transitions == before
            console(servers["survival"], f"pardon {name}")
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline and name in (directories["survival"] / "banned-players.json").read_text():
                time.sleep(0.05)
            chunks, positions = client.chunks, client.positions
            client.command("server survival")
            client.ready(4, chunks, positions)
            verify_backend(client, servers["survival"], directories["survival"] / "server.log", "survival-before-failure")
            chunks, positions = client.chunks, client.positions
            servers["survival"].kill()
            servers["survival"].wait(timeout=20)
            client.ready(5, chunks, positions)
            verify_backend(client, servers["lobby"], directories["lobby"] / "server.log", "lobby-recovered")
            print(f"PASS real vanilla {label}: explicit target ban retained lobby; killed backend recovered to lobby", flush=True)
            console(servers["lobby"], f"ban {name} terminal network ban")
            reason = client.until("disconnect")
            assert b"ban" in reason.lower(), reason
            assert len(client.joins) == 5
            assert client.pack_pops == (4 if protocol >= 765 else 0)
            assert client.transitions == 4
            assert client.respawns == (4 if protocol < 764 else 0)
            print(f"PASS real vanilla {label}: explicit play ban disconnects without recovery", flush=True)
        assert proxy.poll() is None
    print(f"All two-server vanilla {label} checks passed; logs: {directory}", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/release/rift")
    parser.add_argument("--server", action="append", choices=SWITCHABLE_SERVERS,
                        help="repeat to select fixtures; default: every switchable version")
    parser.add_argument("--accept-eula", action="store_true")
    args = parser.parse_args()
    if not __debug__:
        parser.error("assertions must be enabled")
    if not args.accept_eula:
        parser.error("--accept-eula is required to run the Minecraft servers")
    for fixture_name in args.server or SWITCHABLE_SERVERS:
        check(args.binary.resolve(), fixture_name)
