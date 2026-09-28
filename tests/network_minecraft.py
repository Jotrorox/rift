#!/usr/bin/env python3
"""Run the two-server acceptance scenario against pinned vanilla 1.21.11 jars.

Uses one compressed frontend socket throughout lobby -> survival -> lobby,
then kills survival and checks recovery to lobby. Also verifies actual backend
bans and unsigned chat after every transition. Requires Java 21+ and explicit
--accept-eula. Artifacts/logs are retained under target/minecraft/runs/.
"""

import argparse
import io
from contextlib import ExitStack
from pathlib import Path
import struct
import tempfile
import time
import uuid

from minecraft import (CACHE, ROOT, configure_server, process, read_varint, status_ready,
                       string, unused_port, wait_ready)
from network_wire import NetworkClient, read_string


class RealClient(NetworkClient):
    def __init__(self, port, name):
        super().__init__(port, name, chat=False)
        self.chunks = self.positions = self.keepalives = 0
        self.gamemodes, self.borders = [], []

    def next_packet(self):
        event, value = super().next_packet()
        if event == "packet" and self.phase == "play":
            packet_id, body = value
            if packet_id == 0x2C:
                self.chunks += 1
            elif packet_id == 0x46:
                self.positions += 1
            elif packet_id == 0x2B:
                self.keepalives += 1
            elif packet_id == 0x2A:
                self.borders.append(struct.unpack(">dddd", body[:32])[3])
            elif packet_id == 0x30:
                data = io.BytesIO(body)
                data.read(5)  # Entity ID and hardcore flag.
                for _ in range(read_varint(data)):
                    read_string(data)
                for _ in range(3):
                    read_varint(data)  # Player limit, view and simulation distance.
                data.read(3)
                read_varint(data)  # Dimension type.
                assert read_string(data) == "minecraft:overworld"
                data.read(8)  # Seed hash.
                self.gamemodes.append(data.read(1)[0])
        return event, value

    def ready(self, generation, previous_chunks, previous_positions):
        for _ in range(10000):
            if (len(self.joins) >= generation and self.chunks > previous_chunks
                    and self.positions > previous_positions):
                return
            event, body = self.next_packet()
            assert event != "disconnect", body
        raise AssertionError("backend did not deliver a new world, teleport and chunks")

    def chat_message(self, value):
        # Offline fixtures intentionally use unsigned messages. The session's
        # signed-key/chain boundary is independently exercised by network_wire.
        self.send(0x08, string(value) + struct.pack(">qq", int(time.time() * 1000), 0)
                  + b"\0\0\0\0\0\1")


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


def check(binary):
    runs = CACHE / "runs"
    runs.mkdir(parents=True, exist_ok=True)
    directory = Path(tempfile.mkdtemp(prefix="network-vanilla-", dir=runs))
    print(f"Network acceptance logs: {directory}", flush=True)
    ports = {}
    while len(set(ports.values())) < 3:
        ports = {name: unused_port() for name in ("lobby", "survival", "proxy")}
    directories = {name: directory / name for name in ("lobby", "survival")}
    with ExitStack() as stack:
        servers = {}
        for name in ("lobby", "survival"):
            directories[name].mkdir()
            command = configure_server("vanilla", directories[name], ports[name], name == "lobby",
                                       motd=f"rift-network-{name}")
            if name == "survival":
                properties = directories[name] / "server.properties"
                properties.write_text(properties.read_text().replace("gamemode=creative", "gamemode=survival"))
            servers[name] = stack.enter_context(process(command, directories[name], "server.log", server=True))
        for name, server in servers.items():
            wait_ready(server, lambda name=name: status_ready(ports[name], 774),
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
        with RealClient(ports["proxy"], name) as client:
            client.deadline = time.monotonic() + 240
            client.ready(1, 0, 0)
            verify_backend(client, servers["lobby"], directories["lobby"] / "server.log", "lobby-initial")
            with RealClient(ports["proxy"], name) as duplicate:
                assert b"already connected" in duplicate.until("disconnect")
            verify_backend(client, servers["lobby"], directories["lobby"] / "server.log", "lobby-after-duplicate")
            assert f"{name} lost connection" not in (directories["lobby"] / "server.log").read_text()
            for generation, (command, target) in enumerate(
                    [("server survival", "survival"), ("hub", "lobby")], start=2):
                chunks, positions = client.chunks, client.positions
                client.command(command)
                client.ready(generation, chunks, positions)
                verify_backend(client, servers[target], directories[target] / "server.log", target)
            assert client.transitions == 2
            assert client.threshold == 256, "frontend compression changed with backend"
            assert client.compressed_packets and client.sent_compressed_packets
            print("PASS real vanilla: one socket, lobby -> survival -> lobby, fresh worlds/chunks, chat", flush=True)
            # Target login ban must keep the player connected to the old lobby.
            console(servers["survival"], f"ban {name} network acceptance ban")
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline and "network acceptance ban" not in (directories["survival"] / "banned-players.json").read_text():
                time.sleep(0.05)
            before = client.transitions
            client.command("server survival")
            client.until("message", b"ban")
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
            print("PASS real vanilla: explicit target ban retained lobby; killed backend recovered to lobby", flush=True)
            console(servers["lobby"], f"ban {name} terminal network ban")
            reason = client.until("disconnect")
            assert b"ban" in reason.lower(), reason
            assert len(client.joins) == 5
            print("PASS real vanilla: explicit play ban disconnects without recovery", flush=True)
        assert proxy.poll() is None
    print(f"All two-server vanilla checks passed; logs: {directory}", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/rift")
    parser.add_argument("--accept-eula", action="store_true")
    args = parser.parse_args()
    if not __debug__:
        parser.error("assertions must be enabled")
    if not args.accept_eula:
        parser.error("--accept-eula is required to run the Minecraft servers")
    check(args.binary.resolve(strict=True))
