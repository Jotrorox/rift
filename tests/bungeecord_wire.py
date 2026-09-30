#!/usr/bin/env python3
"""BungeeCord plugin-message acceptance with independent Minecraft peers.

Uses Java DataOutput.writeUTF-compatible payloads over real compressed sockets;
no Minecraft server, Java plugin, network download or Rift codec is required.
"""

import argparse
from contextlib import ExitStack, contextmanager
import io
from pathlib import Path
import struct
import tempfile
import time

from minecraft import ROOT, process, string, unused_port, wait_ready
from network_wire import Backend, NetworkClient, identity, read_string


CHANNEL = "bungeecord:main"
ALIASES = (CHANNEL, "BungeeCord")


class PluginClient(NetworkClient):
    def next_packet(self):
        result = super().next_packet()
        # The shared client normally starts on a compressed lobby. This test
        # also starts with a negative (disabled) signed compression threshold.
        if self.threshold is not None and self.threshold >= 1 << 31:
            self.threshold = None
        return result


def write_utf(value):
    """Java modified UTF-8: byte count, encoded NUL, and UTF-16 surrogate pairs."""
    encoded = bytearray()
    utf16 = value.encode("utf-16-be", errors="surrogatepass")
    for offset in range(0, len(utf16), 2):
        unit = int.from_bytes(utf16[offset:offset + 2], "big")
        if 0 < unit <= 0x7f:
            encoded.append(unit)
        elif unit <= 0x7ff:
            encoded.extend((0xc0 | unit >> 6, 0x80 | unit & 0x3f))
        else:
            encoded.extend((0xe0 | unit >> 12, 0x80 | unit >> 6 & 0x3f,
                            0x80 | unit & 0x3f))
    return struct.pack(">H", len(encoded)) + encoded


def fields(*values):
    return b"".join(write_utf(value) for value in values)


def eventually(predicate, description, timeout=5):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = predicate()
        if result:
            return result
        time.sleep(0.01)
    raise AssertionError(f"timed out waiting for {description}")


def connection(backend, name):
    return eventually(lambda: next((record for record in reversed(backend.connections)
                                    if record["name"] == name and record["ready"]), None),
                      f"{name} attached to {backend.name}")


def payloads(record, start=0):
    result = []
    for packet_id, body in record["packets"][start:]:
        if packet_id == 0x15:
            data = io.BytesIO(body)
            result.append((read_string(data), data.read()))
    return result


def send(record, payload, channel=CHANNEL):
    record["peer"].send(0x18, string(channel) + payload)


def query(record, request, expected, channel=CHANNEL):
    start = len(record["packets"])
    send(record, request, channel)
    replies = eventually(lambda: [(name, body) for name, body in payloads(record, start)
                                 if name in ALIASES], f"reply to {request!r}")
    assert replies == [(CHANNEL, expected)], (request, replies, expected)


def registered(record):
    eventually(lambda: ("minecraft:register", CHANNEL.encode()) in payloads(record),
               "proxy channel registration at backend")


def barrier(client, record):
    """Observe every client payload before an ordered backend marker."""
    marker = string("rift:bungee_barrier") + b"complete"
    record["peer"].send(0x18, marker)
    while True:
        kind, body = client.next_packet()
        assert kind != "disconnect", body
        if kind != "payload":
            continue
        if body == marker:
            return
        data = io.BytesIO(body)
        assert read_string(data) not in ALIASES, "compatibility request leaked to client"


@contextmanager
def network(binary, backends, enabled=True, omit=False, draining=()):
    with tempfile.TemporaryDirectory(prefix="rift-bungeecord-") as tmp:
        directory = Path(tmp)
        port = unused_port()
        addresses = ",".join(f"{name}='127.0.0.1:{backend.port}'"
                             for name, backend in backends.items())
        option = "" if omit else "bungeecord=" + str(enabled).lower() + ","
        access = "survival={deny={'Eve'}}" if "survival" in backends else ""
        config = directory / "bungeecord.lua"
        # No initial/hubs list: compatibility must activate protocol handling
        # and transfer capability independently of the built-in commands.
        drained = ",".join(repr(name) for name in draining)
        config.write_text("return {listeners={public='127.0.0.1:" + str(port)
                          + "'},backends={" + addresses + ",dns='localhost:25565',"
                          + "ipv6='[::1]:25566'},routes={public='lobby'},"
                          + "draining={" + drained + "},"
                          + "limits={connect_timeout_ms=1500},network={" + option
                          + "access={" + access + "}}}")
        log = directory / "proxy.log"
        with process([str(binary), "--config", str(config)], directory, "proxy.log") as proxy:
            try:
                wait_ready(proxy, lambda: "rift:" in log.read_text(), log)
                yield port
                assert proxy.poll() is None, log.read_text()
            except Exception:
                print(log.read_text(), flush=True)
                raise


def enabled_checks(binary, threshold):
    with ExitStack() as stack:
        lobby = Backend("lobby", 11, threshold=threshold)
        stack.callback(lobby.close)
        survival = Backend("survival", 22, threshold=-1 if threshold >= 0 else 0)
        stack.callback(survival.close)
        port = stack.enter_context(network(binary, dict(lobby=lobby, survival=survival)))
        alice = stack.enter_context(PluginClient(port, "Alice"))
        alice.joined("lobby")
        bob = stack.enter_context(PluginClient(port, "Bob"))
        bob.joined("lobby")
        eve = stack.enter_context(PluginClient(port, "Eve"))
        eve.joined("lobby")
        source = connection(lobby, "Alice")
        registered(source)

        for channel in ALIASES:
            query(source, fields("GetServer"), fields("GetServer", "lobby"), channel)
            query(source, fields("GetServers"), fields("GetServers", "dns, ipv6, lobby, survival"), channel)
            query(source, fields("PlayerCount", "ALL"), fields("PlayerCount", "ALL")
                  + struct.pack(">i", 3), channel)
            query(source, fields("PlayerCount", "LOBBY"), fields("PlayerCount", "lobby")
                  + struct.pack(">i", 3), channel)
            query(source, fields("PlayerCount", "survival"), fields("PlayerCount", "survival")
                  + struct.pack(">i", 0), channel)
            query(source, fields("PlayerList", "ALL"), fields("PlayerList", "ALL", "Alice, Bob, Eve"), channel)
            query(source, fields("PlayerList", "LOBBY"), fields("PlayerList", "lobby", "Alice, Bob, Eve"), channel)
            query(source, fields("PlayerList", "survival"), fields("PlayerList", "survival", ""), channel)
            query(source, fields("UUID"), fields("UUID", identity("Alice").hex), channel)
            query(source, fields("UUIDOther", "bOb"), fields("UUIDOther", "Bob", identity("Bob").hex), channel)
            query(source, fields("GetPlayerServer", "bOb"), fields("GetPlayerServer", "Bob", "lobby"), channel)
            query(source, fields("IP"), fields("IP", "127.0.0.1")
                  + struct.pack(">i", alice.frontend[1]), channel)
            query(source, fields("IPOther", "bOb"), fields("IPOther", "Bob", "127.0.0.1")
                  + struct.pack(">i", bob.frontend[1]), channel)
            for name, host, backend_port in (("lObBy", "127.0.0.1", lobby.port),
                                             ("dns", "localhost", 25565), ("ipv6", "::1", 25566)):
                query(source, fields("ServerIP", name), fields("ServerIP", name.lower(), host)
                      + struct.pack(">H", backend_port), channel)
        barrier(alice, source)

        # Invalid, unknown, and unavailable queries stay inside the proxy. A
        # valid response after them is an ordered check that handling continues.
        start = len(source["packets"])
        invalid = [b"", b"\0", b"\0\xffx", fields("PlayerCount"),
                   fields("Connect", "survival") + b"trailing",
                   fields("Unknown", "ignored"), fields("Unknown\0😀"),
                   fields("UUIDOther", "absent"), fields("IPOther", "absent"),
                   fields("GetPlayerServer", "absent"), fields("ServerIP", "absent"),
                   fields("PlayerCount", "absent"), fields("PlayerList", "absent"),
                   fields("PlayerCount", "all"), fields("PlayerList", "all")]
        for channel in ALIASES:
            for request in invalid:
                send(source, request, channel)
        query(source, fields("GetServer"), fields("GetServer", "lobby"))
        barrier(alice, source)
        assert [(channel, body) for channel, body in payloads(source, start) if channel in ALIASES] == [
            (CHANNEL, fields("GetServer", "lobby"))]
        assert not survival.connections, "malformed Connect transferred the player"

        # Client spoofing cannot execute a transfer, query the proxy, or reach
        # backend plugin listeners. Normal plugin messages still pass both ways.
        start = len(source["packets"])
        for channel in ALIASES:
            for request in (fields("Connect", "survival"), fields("GetServer"),
                            fields("PlayerCount", "ALL") + struct.pack(">i", 999)):
                alice.send(0x15, string(channel) + request)
        alice.probe("lobby")
        barrier(alice, source)
        assert not [payload for payload in payloads(source, start) if payload[0] in ALIASES]
        assert not survival.connections, "client-originated transfer was accepted"
        ordinary = b"arbitrary\0payload" * 100
        send(source, ordinary, "example:ordinary")
        assert alice.until("payload", string("example:ordinary")) == string("example:ordinary") + ordinary
        alice.send(0x15, string("example:ordinary") + ordinary)
        alice.probe("lobby")
        assert ("example:ordinary", ordinary) in payloads(source)

        # ConnectOther must operate on the named player, including their access
        # rules, and must leave the plugin's carrier attached to the source.
        send(source, fields("ConnectOther", "bOb", "SURVIVAL"), "BungeeCord")
        bob.joined("survival")
        bob.probe("survival")
        registered(connection(survival, "Bob"))
        alice.probe("lobby")
        query(source, fields("GetPlayerServer", "Bob"), fields("GetPlayerServer", "Bob", "survival"))
        query(source, fields("PlayerCount", "lobby"), fields("PlayerCount", "lobby") + struct.pack(">i", 2))
        query(source, fields("PlayerList", "survival"), fields("PlayerList", "survival", "Bob"))
        send(source, fields("ConnectOther", "Eve", "survival"))
        eve_record = connection(lobby, "Eve")
        send(eve_record, fields("Connect", "survival"))
        # No response exists for Connect. Keep all sockets flowing while an
        # erroneously accepted asynchronous transfer has time to be observed.
        for _ in range(10):
            query(source, fields("GetPlayerServer", "Eve"), fields("GetPlayerServer", "Eve", "lobby"))
            eve.probe("lobby")
            time.sleep(0.02)
        assert not [record for record in survival.connections if record["name"] == "Eve"]

        send(source, fields("Connect", "SURVIVAL"))
        alice.joined("survival")
        alice.probe("survival")
        moved = connection(survival, "Alice")
        registered(moved)
        query(moved, fields("GetServer"), fields("GetServer", "survival"))
        query(moved, fields("PlayerList", "survival"), fields("PlayerList", "survival", "Alice, Bob"))
        query(moved, fields("PlayerCount", "ALL"), fields("PlayerCount", "ALL") + struct.pack(">i", 3))
        send(moved, fields("Connect", "lobby"))
        alice.joined("lobby")
        alice.probe("lobby")
        registered(connection(lobby, "Alice"))
        assert alice.joins == [11, 22, 11], alice.joins
        assert bob.joins == [11, 22], bob.joins
        assert eve.joins == [11], eve.joins


def disabled_checks(binary, omit):
    with ExitStack() as stack:
        lobby = Backend("lobby", 11, threshold=256)
        stack.callback(lobby.close)
        port = stack.enter_context(network(binary, dict(lobby=lobby), enabled=False, omit=omit))
        client = stack.enter_context(PluginClient(port))
        client.joined("lobby")
        source = connection(lobby, "Alice")
        for channel in ALIASES:
            request = fields("GetServer")
            send(source, request, channel)
            assert client.until("payload", string(channel)) == string(channel) + request
            client.send(0x15, string(channel) + request)
            client.probe("lobby")
            assert (channel, request) in payloads(source)
        assert not [(channel, body) for channel, body in payloads(source)
                    if channel == "minecraft:register" and CHANNEL.encode() in body]


def rejected_transfer_checks(binary):
    with ExitStack() as stack:
        lobby = Backend("lobby", 11)
        stack.callback(lobby.close)
        banned = Backend("banned", 22, login_ban="Banned by destination")
        stack.callback(banned.close)
        drained = Backend("drained", 33)
        stack.callback(drained.close)
        port = stack.enter_context(network(binary, dict(lobby=lobby, banned=banned, drained=drained),
                                           draining=("drained",)))
        client = stack.enter_context(PluginClient(port))
        client.joined("lobby")
        source = connection(lobby, "Alice")
        send(source, fields("Connect", "banned"))
        eventually(lambda: banned.connections and banned.connections[0]["name"] == "Alice",
                   "destination login denial")
        eventually(lambda: banned.connections[0]["socket"].fileno() == -1,
                   "rejected destination connection closed")
        client.probe("lobby")
        query(source, fields("GetServer"), fields("GetServer", "lobby"))
        for request in (fields("Connect", "drained"), fields("Connect", "missing"),
                        fields("ConnectOther", "missing", "lobby")):
            send(source, request)
        for _ in range(10):
            query(source, fields("GetServer"), fields("GetServer", "lobby"))
            client.probe("lobby")
            time.sleep(0.02)
        assert not drained.connections, "draining destination accepted a transfer"
        assert client.joins == [11] and client.transitions == 0


def check(binary):
    assert write_utf("\0😀") == b"\0\x08\xc0\x80\xed\xa0\xbd\xed\xb8\x80"
    for threshold in (-1, 0, 256):
        enabled_checks(binary, threshold)
    disabled_checks(binary, omit=False)
    disabled_checks(binary, omit=True)
    rejected_transfer_checks(binary)
    print("BungeeCord compatibility wire checks passed (compression off/0/256, aliases, queries, transfers, trust boundary, opt-in)")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target" / "release" / "rift")
    check(parser.parse_args().binary.resolve())
