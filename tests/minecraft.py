#!/usr/bin/env python3
"""Real Minecraft 1.21.11 integration tests; Python standard library + Java 21.

Downloads pinned official jars, verifies checksums, and runs isolated loopback
servers. Packet layouts: https://github.com/PrismarineJS/minecraft-data/tree/master/data/pc/1.21.11
"""

import argparse
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
import hashlib
import io
import json
from pathlib import Path
import socket
import struct
import subprocess
import time
import urllib.request
import uuid
import zlib

ROOT = Path(__file__).resolve().parents[1]
CACHE = ROOT / "target" / "minecraft"
PROTOCOL = 774
JARS = {
    "vanilla": (
        "https://piston-data.mojang.com/v1/objects/64bb6d763bed0a9f1d632ec347938594144943ed/server.jar",
        "sha1", "64bb6d763bed0a9f1d632ec347938594144943ed",
    ),
    "paper": (
        "https://fill-data.papermc.io/v1/objects/5ffef465eeeb5f2a3c23a24419d97c51afd7dbb4923ff42df9a3f58bba1ccfba/paper-1.21.11-132.jar",
        "sha256", "5ffef465eeeb5f2a3c23a24419d97c51afd7dbb4923ff42df9a3f58bba1ccfba",
    ),
}


def varint(value):
    result = bytearray()
    while value > 127:
        result.append((value & 127) | 128)
        value >>= 7
    result.append(value)
    return bytes(result)


def read_varint(stream):
    result = 0
    for shift in range(0, 35, 7):
        byte = stream.read(1)
        if not byte:
            raise EOFError("connection closed mid-packet")
        result |= (byte[0] & 127) << shift
        if not byte[0] & 128:
            return result
    raise ValueError("invalid VarInt")


def string(value):
    encoded = value.encode()
    return varint(len(encoded)) + encoded


class Client:
    def __init__(self, port, state):
        self.socket = socket.create_connection(("127.0.0.1", port), timeout=20)
        self.socket.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.reader = self.socket.makefile("rb")
        self.threshold = None
        self.compressed_packets = 0
        self.send(0, varint(PROTOCOL) + string("localhost") + struct.pack(">H", port) + varint(state))

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.reader.close()
        self.socket.close()

    def send(self, packet_id, body=b""):
        packet = varint(packet_id) + body
        if self.threshold is not None:
            packet = (varint(len(packet)) + zlib.compress(packet)
                      if len(packet) >= self.threshold else b"\0" + packet)
        self.socket.sendall(varint(len(packet)) + packet)

    def receive(self):
        length = read_varint(self.reader)
        assert 0 < length <= 8 * 1024 * 1024, length
        data = self.reader.read(length)
        if len(data) != length:
            raise EOFError("truncated packet")
        if self.threshold is not None:
            framed = io.BytesIO(data)
            uncompressed = read_varint(framed)
            data = framed.read()
            if uncompressed:
                data = zlib.decompress(data)
                assert len(data) == uncompressed
                self.compressed_packets += 1
        packet = io.BytesIO(data)
        packet_id = read_varint(packet)
        return packet_id, packet.read()


def status(port):
    with Client(port, 1) as client:
        client.send(0)
        packet_id, body = client.receive()
        assert packet_id == 0
        data = io.BytesIO(body)
        result = json.loads(data.read(read_varint(data)))
        payload = struct.pack(">q", 123456789012345)
        client.send(1, payload)
        assert client.receive() == (1, payload), "ping payload changed"
        return result


def play(port, name):
    # An offline-mode fixture avoids needing an actual Microsoft account/token.
    digest = hashlib.md5(f"OfflinePlayer:{name}".encode()).digest()
    player_id = uuid.UUID(bytes=digest, version=3)
    with Client(port, 2) as client:
        client.send(0, string(name) + player_id.bytes)
        while True:
            packet_id, body = client.receive()
            if packet_id == 3:
                client.threshold = read_varint(io.BytesIO(body))
            elif packet_id == 2:
                assert body[:16] == player_id.bytes
                break
            else:
                raise AssertionError(f"unexpected login packet {packet_id}: {body[:200]!r}")
        client.send(3)  # Login acknowledged; enter configuration.
        client.send(0, string("en_us") + bytes([2, 0, 1, 127, 1, 0, 1, 2]))
        # This also exercises compression in the client -> server direction.
        client.send(2, string("minecraft:brand") + string("rift-test" * 28))
        while True:
            packet_id, body = client.receive()
            if packet_id == 0x0E:
                client.send(7, b"\0")  # No cached packs: request full registries.
            elif packet_id in (4, 5):
                client.send(packet_id, body)  # Keepalive / ping.
            elif packet_id == 3:
                client.send(3)
                break
            elif packet_id == 2:
                raise AssertionError(f"configuration disconnect: {body[:200]!r}")
        joined = positioned = False
        chunks = 0
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            packet_id, body = client.receive()
            if packet_id == 0x30:
                joined = True
            elif packet_id == 0x46:
                teleport = read_varint(io.BytesIO(body))
                client.send(0, varint(teleport))
                client.send(0x2B)  # Player loaded.
                positioned = True
            elif packet_id == 0x2C:
                chunks += 1
            elif packet_id == 0x0B:
                client.send(0x0A, struct.pack(">f", 10.0))
            elif packet_id == 0x2B:
                client.send(0x1B, body)
                if joined and positioned and chunks:
                    assert client.compressed_packets > 0
                    return chunks, client.compressed_packets
            elif packet_id == 0x20:
                raise AssertionError(f"play disconnect: {body[:200]!r}")
        raise AssertionError(f"incomplete play: joined={joined}, positioned={positioned}, chunks={chunks}")


def unused_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


@contextmanager
def process(command, directory, log_name, server=False):
    with (directory / log_name).open("w") as log:
        proc = subprocess.Popen(command, cwd=directory, stdin=subprocess.PIPE,
                                stdout=log, stderr=subprocess.STDOUT, text=True)
        try:
            yield proc
        finally:
            if proc.poll() is None:
                if server:
                    try:
                        proc.stdin.write("stop\n")
                        proc.stdin.flush()
                    except BrokenPipeError:
                        pass
                else:
                    proc.terminate()
                try:
                    proc.wait(timeout=30)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait()
            proc.stdin.close()


def wait_ready(proc, ready, log):
    deadline = time.monotonic() + 180
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            raise RuntimeError(f"process exited ({proc.returncode}): {log}\n{log.read_text()[-4000:]}")
        if ready():
            return
        time.sleep(0.1)
    raise TimeoutError(f"startup timed out: {log}\n{log.read_text()[-4000:]}")


def download(name):
    url, algorithm, expected = JARS[name]
    jar = CACHE / f"{name}.jar"
    if not jar.exists():
        print(f"Downloading {name}...", flush=True)
        request = urllib.request.Request(url, headers={
            "User-Agent": "rift-integration-tests/0.1.0 (https://github.com/Jotrorox/rift)"
        })
        with urllib.request.urlopen(request, timeout=120) as response:
            content = response.read()
        assert hashlib.new(algorithm, content).hexdigest() == expected, "jar checksum mismatch"
        jar.write_bytes(content)
    assert hashlib.new(algorithm, jar.read_bytes()).hexdigest() == expected, "cached jar checksum mismatch"
    return jar


def test_server(name, binary):
    jar = download(name)
    directory = CACHE / name
    directory.mkdir(exist_ok=True)
    backend, frontend = unused_port(), unused_port()
    while frontend == backend:
        frontend = unused_port()
    (directory / "eula.txt").write_text("eula=true\n")
    (directory / "server.properties").write_text(
        f"server-ip=127.0.0.1\nserver-port={backend}\nmotd=rift-{name}-test\n"
        "online-mode=false\nenforce-secure-profile=false\nnetwork-compression-threshold=256\n"
        "gamemode=creative\nforce-gamemode=true\ndifficulty=peaceful\n"
        "view-distance=2\nsimulation-distance=2\nmax-players=20\nlevel-type=minecraft:flat\n"
        'generator-settings={"layers":[{"block":"minecraft:bedrock","height":1},'
        '{"block":"minecraft:dirt","height":2},{"block":"minecraft:grass_block","height":1}],'
        '"biome":"minecraft:plains"}\n'
        "generate-structures=false\nspawn-protection=0\nmax-tick-time=-1\n"
        "pause-when-empty-seconds=0\n"
    )
    proxy_log = directory / "proxy.log"
    server_log = directory / "server.log"
    with process([str(binary), f"127.0.0.1:{frontend}", f"127.0.0.1:{backend}"],
                 directory, "proxy.log") as proxy:
        wait_ready(proxy, lambda: "rift:" in proxy_log.read_text(), proxy_log)
        # A failed backend connection must close the client and leave Rift alive.
        with socket.create_connection(("127.0.0.1", frontend), timeout=5) as client:
            assert client.recv(1) == b""
        print(f"Starting {name} 1.21.11...", flush=True)
        with process(["java", "-XX:ActiveProcessorCount=2", "-Xms256M", "-Xmx768M",
                      "-jar", str(jar), "nogui"], directory, "server.log", server=True) as server:
            wait_ready(server, lambda: 'Done (' in server_log.read_text(), server_log)
            direct, proxied = status(backend), status(frontend)
            assert direct == proxied, (direct, proxied)
            assert direct["version"]["protocol"] == PROTOCOL
            assert f"rift-{name}-test" in json.dumps(proxied["description"])
            with ThreadPoolExecutor(max_workers=16) as pool:
                responses = list(pool.map(status, [frontend] * 64))
            assert all(item["version"] == direct["version"] for item in responses)
            print(f"PASS {name}: backend recovery, status equality, ping, 64 status requests (16 concurrent)", flush=True)
            # Fresh players avoid saved health/location affecting repeat runs.
            for port, prefix in [(backend, "RiftD"), (frontend, "RiftP")]:
                player = prefix + uuid.uuid4().hex[:8]
                chunks, compressed = play(port, player)
                route = "direct" if port == backend else "proxied"
                print(f"PASS {name} {route}: login, compression, configuration, {chunks} chunks, keepalive ({compressed} compressed packets)", flush=True)
            assert proxy.poll() is None
        assert proxy.poll() is None
        with socket.create_connection(("127.0.0.1", frontend), timeout=5) as client:
            assert client.recv(1) == b""
        print(f"PASS {name}: backend shutdown handled", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--accept-eula", action="store_true",
                        help="accept https://aka.ms/MinecraftEULA for the local test servers")
    args = parser.parse_args()
    if not args.accept_eula:
        parser.error("--accept-eula is required to run the Minecraft servers")
    CACHE.mkdir(parents=True, exist_ok=True)
    subprocess.run(["cargo", "build", "--release", "--locked"], cwd=ROOT, check=True)
    for name in JARS:
        test_server(name, ROOT / "target" / "release" / "rift")
    print("All Minecraft integration tests passed.", flush=True)


if __name__ == "__main__":
    main()
