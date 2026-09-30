#!/usr/bin/env python3
"""Verify Rift against independent Python/zlib peers; no game server or downloads.

Exercises the control packet layouts used by the session layer, including
pre-configuration login, version-specific acknowledgements and disconnects.
"""

import argparse
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
import random
import socket
import subprocess
import time
import uuid

from minecraft import Client, PROTOCOLS, ROOT, login_start, string, varint

PLAYER_ID = uuid.UUID("00000000-0000-0000-0000-000000000001")
PAYLOAD = random.Random(2026).randbytes(40000) + b"Minecraft compression" * 4000
VERSIONS = tuple(PROTOCOLS)


def backend_session(listener, protocol, threshold):
    sock, _ = listener.accept()
    peer = Client.__new__(Client)
    peer.socket = sock
    sock.settimeout(10)
    peer.reader = sock.makefile("rb")
    peer.threshold = None
    peer.compressed_packets = peer.sent_compressed_packets = 0
    peer.deadline = time.monotonic() + 20
    try:
        assert peer.receive()[0] == 0  # Handshake.
        assert peer.receive()[0] == 0  # Login start.
        peer.send(3, varint(threshold & 0xFFFFFFFF))
        peer.threshold = threshold if threshold >= 0 else None
        identity = string(str(PLAYER_ID)) if protocol < 735 else PLAYER_ID.bytes
        success = identity + string("Interop") + (b"\0" if protocol >= 759 else b"")
        if protocol >= 776:
            success += uuid.UUID(int=2).bytes  # Backend session UUID, added in 26.2.
        if protocol in (766, 767):
            success += b"\0"  # Strict error handling, only present in these versions.
        peer.send(2, success)
        if protocol >= 764:
            assert peer.receive() == (3, b"")
            finish = 2 if protocol < 766 else 3
            peer.send(finish)
            assert peer.receive() == (finish, b"")
        peer.send(0x7F, PAYLOAD)
        assert peer.receive() == (0x7F, PAYLOAD)
    finally:
        peer.reader.close()
        sock.close()


def check_client(port, protocol, threshold):
    with Client(port, 2, protocol) as client:
        client.send(0, login_start("Interop", PLAYER_ID, protocol))
        assert client.receive()[0] == 3
        client.threshold = threshold if threshold >= 0 else None
        assert client.receive()[0] == 2
        if protocol >= 764:
            client.send(3)
            finish = 2 if protocol < 766 else 3
            assert client.receive() == (finish, b"")
            client.send(finish)
        assert client.receive() == (0x7F, PAYLOAD)
        client.send(0x7F, PAYLOAD)
        kick, reason = client.receive()
        expected = PROTOCOLS[protocol]["disconnect"]
        assert kick == expected, (protocol, kick, expected)
        assert b"connection was lost" in reason


def check(binary):
    with socket.socket() as backend:
        backend.bind(("127.0.0.1", 0))
        backend.listen()
        backend.settimeout(10)
        proxy = subprocess.Popen(
            [str(binary), "127.0.0.1:0", f"127.0.0.1:{backend.getsockname()[1]}"],
            stderr=subprocess.PIPE, text=True,
        )
        try:
            ready = proxy.stderr.readline()
            assert ready.startswith("rift: listening on "), ready
            port = int(ready.split(" -> ")[0].rsplit(":", 1)[1])
            with ThreadPoolExecutor(max_workers=1) as pool:
                for protocol in VERSIONS:
                    for threshold in (-1, 0, 64, 256):
                        server = pool.submit(backend_session, backend, protocol, threshold)
                        check_client(port, protocol, threshold)
                        server.result(timeout=10)
            print(f"PASS: {len(VERSIONS) * 4} wire sessions, {len(VERSIONS)} protocol versions, four compression thresholds; "
                  "Python zlib verified both directions and state-correct disconnects.")
        finally:
            proxy.terminate()
            try:
                proxy.wait(timeout=10)
            except subprocess.TimeoutExpired:
                proxy.kill()
                proxy.wait()
            proxy.stderr.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/release/rift")
    args = parser.parse_args()
    if not __debug__:
        parser.error("assertions must be enabled")
    check(args.binary.resolve())
