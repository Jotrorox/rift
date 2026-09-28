#!/usr/bin/env python3
"""Independent Minecraft 1.21.11 network acceptance peers (no downloads).

Exercises one frontend socket across lobby/survival/lobby and backend loss,
compression changes, configuration acknowledgements, world replacement, chat
reset, access rules, initial order, and explicit login/configuration/play bans.
Layouts: https://github.com/PrismarineJS/minecraft-data/tree/master/data/pc/1.21.11
"""

import argparse
from contextlib import ExitStack, contextmanager
import hashlib
import io
from pathlib import Path
import socket
import struct
import tempfile
import threading
import time
import uuid

from minecraft import (Client, ROOT, process, read_varint, string, teleport_acknowledgement,
                       unused_port, varint, wait_ready)

PROTOCOL = 774
SETTINGS = string("en_us") + bytes([2, 0, 1, 127, 1, 0, 1, 2])
BRAND = string("minecraft:brand") + string("rift-network-test")


def nbt_text(value):
    raw = value.encode()
    return b"\x08" + struct.pack(">H", len(raw)) + raw


def read_string(data):
    return data.read(read_varint(data)).decode()


def identity(name):
    return uuid.UUID(bytes=hashlib.md5(f"OfflinePlayer:{name}".encode()).digest(), version=3)


def join_game(entity_id, world="minecraft:overworld"):
    # Same dimension on every backend deliberately exercises replacing an
    # already loaded overworld, rather than relying on a dimension change.
    spawn = (b"\0" + string(world) + struct.pack(">q", entity_id)
             + bytes([1, 255, 0, 1, 0, 0, 63]))
    return (struct.pack(">i", entity_id) + b"\0\1" + string(world)
            + bytes([20, 2, 2, 0, 1, 0]) + spawn + b"\0")


def chat_session(generation):
    # Valid packet layout; fake peers deliberately do not claim to verify a
    # Mojang signature. The important boundary is fresh session vs stale replay.
    return (uuid.UUID(int=generation + 1).bytes + struct.pack(">q", 4102444800000)
            + b"\3key\3sig")


class Backend:
    def __init__(self, name, entity_id, threshold=256, login_ban=None, config_ban=None,
                 login_fail=False, login_stall=False):
        self.name, self.entity_id, self.threshold = name, entity_id, threshold
        self.login_ban, self.config_ban = login_ban, config_ban
        self.login_fail, self.login_stall = login_fail, login_stall
        self.on_login = None
        self.listener = socket.socket()
        self.listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.listener.bind(("127.0.0.1", 0))
        self.port = self.listener.getsockname()[1]
        self.listener.listen()
        self.listener.settimeout(0.2)
        self.closed = threading.Event()
        self.connections, self.errors, self.workers = [], [], []
        self.thread = threading.Thread(target=self.accept, daemon=True)
        self.thread.start()

    def accept(self):
        while not self.closed.is_set():
            try:
                sock, _ = self.listener.accept()
            except socket.timeout:
                continue
            except OSError:
                break
            record = dict(socket=sock, name=None, packets=[], sessions=[], settings=[], brands=[], ready=False)
            self.connections.append(record)
            worker = threading.Thread(target=self.run, args=(record,), daemon=True)
            self.workers.append(worker)
            worker.start()

    def run(self, record):
        peer = Client.__new__(Client)
        peer.socket = record["socket"]
        peer.socket.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        peer.reader = peer.socket.makefile("rb")
        peer.threshold = None
        peer.compressed_packets = peer.sent_compressed_packets = 0
        peer.deadline = time.monotonic() + 120
        record["peer"] = peer
        try:
            packet_id, handshake = peer.receive()
            assert packet_id == 0
            data = io.BytesIO(handshake)
            assert read_varint(data) == PROTOCOL
            read_string(data)
            assert len(data.read(2)) == 2
            assert read_varint(data) == 2
            packet_id, start = peer.receive()
            assert packet_id == 0
            record["name"] = read_string(io.BytesIO(start))
            if self.on_login:
                self.on_login()
            if self.login_fail:
                return
            if self.login_stall:
                self.closed.wait(3)
            if self.login_ban:
                self.reject(peer, 0, string('{"text":"' + self.login_ban + '"}'))
                return
            peer.send(3, varint(self.threshold & 0xFFFFFFFF))
            peer.threshold = self.threshold if self.threshold >= 0 else None
            peer.send(2, identity(record["name"]).bytes + string(record["name"]) + b"\0")
            assert peer.receive() == (3, b"")
            peer.send(0x0E, b"\0")  # Empty known packs request.
            if self.config_ban:
                # The round trip also ensures the client has finished sending
                # its initial configuration options before receiving the ban.
                while peer.receive()[0] != 7:
                    pass
                self.reject(peer, 2, nbt_text(self.config_ban))
                return
            peer.send(3)
            while True:
                packet_id, body = peer.receive()
                if packet_id == 3:
                    assert not body
                    break
                if packet_id == 0:
                    record["settings"].append(body)
                elif packet_id == 2:
                    record["brands"].append(body)
                elif packet_id != 7:
                    raise AssertionError(f"{self.name}: unexpected configuration packet {packet_id:#x}")
            peer.send(0x30, join_game(self.entity_id))
            peer.send(0x18, string("rift:joined") + string(self.name))
            record["ready"] = True
            while True:
                packet_id, body = peer.receive()
                record["packets"].append((packet_id, body))
                if packet_id == 0x09:
                    record["sessions"].append(body)
                elif packet_id == 0x15:
                    data = io.BytesIO(body)
                    channel = read_string(data)
                    if channel == "rift:probe":
                        peer.send(0x18, string(channel) + string(self.name) + data.read())
                    elif channel == "rift:fail":
                        return
                    elif channel == "rift:ban":
                        self.reject(peer, 0x20, nbt_text("Banned by " + self.name))
                        return
        except (EOFError, ConnectionError, OSError):
            pass  # Client/proxy cleanup and intentional failure close the socket.
        except Exception as error:
            self.errors.append(error)
        finally:
            peer.reader.close()
            peer.socket.close()

    @staticmethod
    def reject(peer, packet_id, body):
        peer.send(packet_id, body)
        # Keep draining queued client options until the proxy closes. Closing
        # immediately with unread input can send RST and discard the very ban
        # packet whose policy handling this fixture is meant to verify.
        peer.socket.shutdown(socket.SHUT_WR)
        while True:
            peer.receive()

    def fail_connections(self):
        for record in self.connections:
            try:
                record["socket"].shutdown(socket.SHUT_RDWR)
            except OSError:
                pass

    def close(self):
        self.closed.set()
        self.listener.close()
        self.fail_connections()
        self.thread.join(timeout=2)
        for worker in self.workers:
            worker.join(timeout=2)
        if self.errors:
            raise AssertionError(f"{self.name}: {self.errors!r}")


class NetworkClient(Client):
    def __init__(self, port, name="Alice", chat=True, bootstrap=True):
        super().__init__(port, 2, PROTOCOL)
        self.name, self.chat = name, chat
        self.bootstrap = bootstrap
        self.phase = "login"
        self.joins, self.transitions, self.pack_pops = [], 0, 0
        self.messages = []
        self.frontend = self.socket.getsockname()
        self.send(0, string(name) + identity(name).bytes)

    def next_packet(self):
        packet_id, body = self.receive()
        if self.phase == "login":
            if packet_id == 0:
                return "disconnect", body
            if packet_id == 3:
                self.threshold = read_varint(io.BytesIO(body))
            elif packet_id == 2:
                assert body[:16] == identity(self.name).bytes
                self.phase = "configuration"
                if self.bootstrap:
                    self.send(3)
                    self.send(0, SETTINGS)
                    self.send(2, BRAND)
                    self.send(2, string("rift:compression_test") + b"x" * 512)
            else:
                raise AssertionError(f"unexpected login packet {packet_id:#x}")
        elif self.phase == "configuration":
            if packet_id == 2:
                return "disconnect", body
            if packet_id == 0x0E:
                self.send(7, b"\0")
            elif packet_id == 3:
                self.send(3)
                self.phase = "play"
            elif packet_id in (4, 5):
                self.send(packet_id, body)
            elif packet_id == 8 and body == b"\0":
                self.pack_pops += 1
        elif self.phase == "play":
            if packet_id == 0x20:
                return "disconnect", body
            if packet_id == 0x74:
                assert body == b""
                self.transitions += 1
                # A legitimate in-flight packet must be drained before ack.
                self.send(0x1B, struct.pack(">q", 999))
                self.send(0x0F)
                self.phase = "configuration"
            elif packet_id == 0x30:
                self.joins.append(struct.unpack(">i", body[:4])[0])
                if self.chat:
                    self.send(0x09, chat_session(len(self.joins)))
            elif packet_id == 0x77:
                self.messages.append(body)
                return "message", body
            elif packet_id == 0x18:
                return "payload", body
            elif packet_id == 0x2B:
                self.send(0x1B, body)
            elif packet_id == 0x46:
                self.send(0, teleport_acknowledgement(body, PROTOCOL))
                self.send(0x2B)
            elif packet_id == 0x0B:
                self.send(0x0A, struct.pack(">f", 10.0))
        assert self.socket.getsockname() == self.frontend, "frontend socket replaced"
        return "packet", (packet_id, body)

    def until(self, kind, expected=None):
        for _ in range(5000):
            event, body = self.next_packet()
            if event == "disconnect" and kind != "disconnect":
                raise AssertionError(f"unexpected disconnect: {body!r}")
            if event == kind and (expected is None or expected in body):
                return body
        raise AssertionError(f"did not receive {kind}: {expected!r}")

    def joined(self, backend):
        self.until("payload", string("rift:joined") + string(backend))

    def command(self, command):
        self.send(0x06, string(command))

    def probe(self, backend):
        payload = string("relay") + b"x" * 1024
        self.send(0x15, string("rift:probe") + payload)
        assert self.until("payload", string("rift:probe")) == (
            string("rift:probe") + string(backend) + payload)


@contextmanager
def network(binary, backends, initial=("lobby",), hubs=("lobby",), access=""):
    with tempfile.TemporaryDirectory(prefix="rift-network-") as tmp:
        directory = Path(tmp)
        port = unused_port()
        config = directory / "network.lua"
        addresses = ",".join(f"['{name}']='127.0.0.1:{backend.port if isinstance(backend, Backend) else backend}'"
                             for name, backend in backends.items())
        names = lambda values: ",".join(repr(item) for item in values)
        config.write_text("return {listeners={public='127.0.0.1:" + str(port)
                          + "'},backends={" + addresses + "},routes={public='lobby'},"
                          + "limits={connect_timeout_ms=1500},network={initial={" + names(initial) + "},hubs={" + names(hubs)
                          + "},access={" + access + "}}}")
        log = directory / "proxy.log"
        with process([str(binary), "--config", str(config)], directory, "proxy.log") as proxy:
            try:
                wait_ready(proxy, lambda: "rift:" in log.read_text(), log)
                yield port
                assert proxy.poll() is None, log.read_text()
            except Exception:
                print(log.read_text(), flush=True)
                raise


def check(binary):
    with ExitStack() as stack:
        def backend(*args, **kwargs):
            result = Backend(*args, **kwargs)
            stack.callback(result.close)
            return result
        lobby = backend("lobby", 11, threshold=256)
        survival = backend("survival", 22, threshold=-1)
        backup = backend("backup", 33, threshold=0)
        banned = backend("banned", 44, login_ban="Explicit login ban")
        config_ban = backend("config_ban", 55, config_ban="Explicit configuration ban")
        failed = backend("failed", 66, login_fail=True)
        stalled = backend("stalled", 77, login_stall=True)
        closed = unused_port()
        targets = dict(lobby=lobby, survival=survival, backup=backup, banned=banned,
                       config_ban=config_ban, offline=closed, failed=failed, stalled=stalled)
        access = "survival={allow={'Alice'},deny={'Eve'}},backup={deny={'Eve'}}"
        with network(binary, targets, access=access) as port:
            with NetworkClient(port) as client:
                client.joined("lobby")
                client.probe("lobby")
                # Signed command arguments must reach the backend unchanged:
                # swallowing them would consume a link in its signature chain.
                signed = (string("server survival") + struct.pack(">qq", 123, 456)
                          + b"\1" + string("server") + b"s" * 256
                          + b"\0\0\0\0\0")
                client.send(0x07, signed)
                client.probe("lobby")
                assert (0x07, signed) in lobby.connections[-1]["packets"]
                assert client.transitions == 0
                # No signed arguments: the proxy can handle the command, but
                # must preserve its acknowledgement offset at the backend.
                client.send(0x07, string("server") + struct.pack(">qq", 124, 457)
                            + b"\0\3\0\0\0\0")
                client.until("message", b"survival")
                client.probe("lobby")
                assert (0x05, b"\3") in lobby.connections[-1]["packets"]
                client.command("server")
                client.until("message", b"survival")
                client.command("server survival")
                client.joined("survival")
                client.probe("survival")
                client.command("hub")
                client.joined("lobby")
                client.probe("lobby")
                assert client.joins == [11, 22, 11], client.joins
                assert client.transitions == 2
                assert client.pack_pops == 2, "old backend resource-pack stack was retained"
                assert client.threshold == 256, "frontend compression renegotiated"
                # An unavailable or explicitly rejecting destination must leave
                # the previous backend usable, with no configuration transition.
                before = client.transitions
                client.command("server offline")
                client.until("message")
                client.probe("lobby")
                client.command("server banned")
                client.until("message", b"ban")
                client.probe("lobby")
                assert client.transitions == before
                client.command("server survival")
                client.joined("survival")
                client.send(0x15, string("rift:fail"))
                client.joined("lobby")
                client.probe("lobby")
                assert client.joins[-2:] == [22, 11]
                # A play kick is an explicit policy decision; never recover it.
                attempts = len(lobby.connections)
                client.send(0x15, string("rift:ban"))
                assert b"Banned by lobby" in client.until("disconnect")
                assert len(lobby.connections) == attempts
            # Every replacement receives settings and a newly generated chat
            # session, never the previous backend's chat key/chain.
            for item in [r for b in (lobby, survival) for r in b.connections if r["ready"]]:
                assert item["settings"] == [SETTINGS], item["settings"]
                assert BRAND in item["brands"], item["brands"]
                assert len(item["sessions"]) == 1, item["sessions"]
                assert (0x1B, struct.pack(">q", 999)) not in item["packets"], "old-world reply leaked"
            observed = [record["sessions"][0] for record in survival.connections if record["ready"]]
            assert observed == [chat_session(2), chat_session(4)], observed
            with NetworkClient(port, "Eve") as client:
                client.joined("lobby")
                count = len(survival.connections)
                client.command("server survival")
                client.until("message")
                client.probe("lobby")
                assert len(survival.connections) == count, "access-denied target was contacted"
                assert client.transitions == 0
                before_duplicate = len(lobby.connections)
                with NetworkClient(port, "Eve", bootstrap=False) as duplicate:
                    assert b"already connected" in duplicate.until("disconnect")
                assert len(lobby.connections) == before_duplicate, "duplicate login reached backend"
                client.probe("lobby")
        print("PASS: lobby/survival/lobby, compression, chat reset, failure recovery, command/access denial, play ban")
        # Initial order excludes restricted entries and proceeds only after
        # transport failure. It must stop on an explicit login rejection.
        with network(binary, targets, initial=("offline", "backup", "lobby"),
                     access="backup={allow={}}") as port:
            before = len(backup.connections)
            with NetworkClient(port) as client:
                client.joined("lobby")
                client.probe("lobby")
            assert len(backup.connections) == before
        with network(binary, targets, initial=("offline", "backup", "lobby")) as port:
            before = len(lobby.connections)
            with NetworkClient(port) as client:
                client.joined("backup")
                client.probe("backup")
            assert len(lobby.connections) == before
        for unavailable in ("failed", "stalled"):
            with network(binary, targets, initial=(unavailable, "lobby")) as port:
                with NetworkClient(port) as client:
                    client.joined("lobby")
                    client.probe("lobby")
        for destination, text in (("banned", b"Explicit login ban"),
                                  ("config_ban", b"Explicit configuration ban")):
            with network(binary, targets, initial=(destination, "lobby")) as port:
                before = len(lobby.connections)
                with NetworkClient(port) as client:
                    reason = client.until("disconnect")
                    assert text in reason, (destination, reason)
                assert len(lobby.connections) == before, "explicit restriction bypassed by fallback"
        with network(binary, targets, hubs=("offline", "backup", "lobby"),
                     access="backup={allow={}}") as port:
            with NetworkClient(port) as client:
                client.joined("lobby")
                client.command("server survival")
                client.joined("survival")
                before = len(backup.connections)
                client.send(0x15, string("rift:fail"))
                client.joined("lobby")
                client.probe("lobby")
                assert len(backup.connections) == before, "recovery bypassed access restriction"
        for banned_hub in ("banned", "config_ban"):
            with network(binary, targets, hubs=(banned_hub, "lobby")) as port:
                with NetworkClient(port) as client:
                    client.joined("lobby")
                    client.command("server survival")
                    client.joined("survival")
                    before = len(lobby.connections)
                    client.send(0x15, string("rift:fail"))
                    client.until("disconnect")
                    assert len(lobby.connections) == before, "recovery bypassed an explicit ban"
        # A target timeout must not erase the old world's readiness: if the
        # old backend dies while target login is pending, recover to a hub.
        stalled.on_login = lobby.fail_connections
        try:
            with network(binary, targets, hubs=("backup",)) as port:
                with NetworkClient(port) as client:
                    client.joined("lobby")
                    client.command("server stalled")
                    client.joined("backup")
                    client.probe("backup")
        finally:
            stalled.on_login = None
        print("PASS: ordered initial/recovery fallback, login EOF/timeout, access rules, terminal bans, overlapping failure")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/rift")
    args = parser.parse_args()
    if not __debug__:
        parser.error("assertions must be enabled")
    check(args.binary.resolve(strict=True))
