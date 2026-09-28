#!/usr/bin/env python3
"""Two real Paper servers, proxy-owned authentication, and modern forwarding.

Requires a signed-in Minecraft Java 1.21.11 client. Never accepts account tokens
or passwords. Every visual/gameplay claim requires an explicit operator PASS.
"""

import argparse
import base64
from contextlib import ExitStack
import hashlib
import hmac
import io
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import subprocess
import tempfile
import time
import traceback
import urllib.request
import uuid
import zipfile

import minecraft as mc
import operations as op


SECRET_ENV = "RIFT_FORWARDING_SECRET"


def encryption_challenge(port, protocol, name="RiftAuthProbe", player_id=None):
    """A claimed name/UUID never bypasses the online encryption challenge."""
    with mc.Client(port, 2, protocol) as client:
        client.send(0, mc.string(name) + (player_id or uuid.uuid4()).bytes)
        packet_id, body = client.receive()
        assert packet_id == 1, f"expected Encryption Request, received {packet_id}"
        data = io.BytesIO(body)
        data.read(mc.read_varint(data))
        key = data.read(mc.read_varint(data))
        challenge = data.read(mc.read_varint(data))
        assert len(key) >= 128 and len(challenge) >= 4, "missing encryption key/challenge"
        assert data.read() == b"\x01", "server did not require authentication"
    return {"encryption_request": True, "should_authenticate": True}


def reject_bad_forwarding(port, protocol):
    """Paper must reject a correctly framed payload signed with the wrong key."""
    player_id = uuid.uuid4()
    with mc.Client(port, 2, protocol) as client:
        client.send(0, mc.string("RiftFwdProbe") + player_id.bytes)
        answered = False
        for _ in range(8):
            packet_id, body = client.receive()
            if packet_id == 4:
                data = io.BytesIO(body)
                query = mc.read_varint(data)
                channel = data.read(mc.read_varint(data)).decode()
                assert channel == "velocity:player_info", channel
                payload = (mc.varint(1) + mc.string("203.0.113.45") + player_id.bytes
                           + mc.string("RiftFwdProbe") + mc.varint(0))
                invalid_mac = hmac.digest(b"deliberately-incorrect-fixture-secret", payload, "sha256")
                client.send(2, mc.varint(query) + b"\x01" + invalid_mac + payload)
                answered = True
            elif packet_id == 3:
                client.threshold = mc.read_varint(io.BytesIO(body))
            elif packet_id == 0:
                assert answered, "Paper rejected before asking for Velocity forwarding"
                reason = io.BytesIO(body)
                return {"rejected": True, "reason": json.loads(reason.read(mc.read_varint(reason)))}
            else:
                raise AssertionError(f"bad forwarding was not rejected: {packet_id}")
    raise AssertionError("Paper did not reject the incorrect forwarding signature")


def confirm(result, step, instruction):
    print(f"\n{instruction}", flush=True)
    if input("Type PASS only after verifying this in the game (anything else fails): ").strip() != "PASS":
        raise AssertionError(f"operator did not confirm {step}")
    result["manual_steps"][step] = {
        "passed": True, "utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())}


def public_profile(name):
    request = urllib.request.Request(
        "https://api.minecraftservices.com/minecraft/profile/lookup/name/" + name,
        headers={"User-Agent": "rift-online-acceptance/1.0"})
    with urllib.request.urlopen(request, timeout=15) as response:
        profile = json.load(response)
    assert profile["name"].casefold() == name.casefold(), "public profile name mismatch"
    profile["id"] = str(uuid.UUID(profile["id"]))
    offline = uuid.UUID(bytes=hashlib.md5(f"OfflinePlayer:{profile['name']}".encode()).digest(), version=3)
    assert profile["id"] != str(offline)
    return profile


def build_probe(directory):
    """Compile against libraries embedded in the checksum-pinned Paper jar."""
    build = directory / "probe-build"
    build.mkdir()
    libraries = build / "libraries"
    libraries.mkdir()
    with zipfile.ZipFile(mc.download("paper")) as paper:
        for entry in paper.infolist():
            if entry.filename.startswith("META-INF/libraries/") and entry.filename.endswith(".jar"):
                (libraries / Path(entry.filename).name).write_bytes(paper.read(entry))
    source = mc.ROOT / "tests/fixtures/online-plugin"
    subprocess.run(["javac", "--release", "21", "-proc:none", "-classpath", str(libraries / "*"),
                    "-d", str(build), str(source / "RiftOnlineProbe.java")], check=True)
    plugin = build / "rift-online-probe.jar"
    with zipfile.ZipFile(plugin, "w") as output:
        output.write(build / "RiftOnlineProbe.class", "RiftOnlineProbe.class")
        output.write(source / "plugin.yml", "plugin.yml")
    return plugin


def forwarding_config(directory, secret):
    config = directory / "config"
    config.mkdir(exist_ok=True)
    path = config / "paper-global.yml"
    path.write_text("_version: 31\nproxies:\n  velocity:\n    enabled: true\n"
                    f"    online-mode: true\n    secret: '{secret}'\n")
    path.chmod(0o600)


def configuration(front, monitor, primary, lobby):
    return f"""return {{
    listeners = {{ public = '127.0.0.1:{front}' }},
    backends = {{ primary = '127.0.0.1:{primary}', lobby = '127.0.0.1:{lobby}' }},
    routes = {{ public = 'lobby' }},
    network = {{ initial = {{'lobby'}}, hubs = {{'lobby'}} }},
    authentication = {{ online_mode = true, timeout_ms = 10000 }},
    forwarding = {{ mode = 'velocity', secret_env = '{SECRET_ENV}' }},
    metrics = '127.0.0.1:{monitor}',
    shutdown_timeout_ms = 1000,
}}
"""


def console(server, command):
    server.stdin.write(command + "\n")
    server.stdin.flush()


def records(directory):
    path = directory / "plugins/RiftOnlineProbe/profiles.jsonl"
    if not path.exists():
        return []
    # Ignore an in-progress final write; the next poll observes its newline.
    return [json.loads(line) for line in path.read_text().splitlines(keepends=True) if line.endswith("\n")]


def snapshot(server, directory, name):
    before = len(records(directory))
    console(server, f"riftprobe {name}")
    return op.eventually(lambda: (rows[-1] if len(rows := records(directory)) > before else None),
                         f"Paper profile/inventory snapshot for {name}", timeout=10)


def verify_profile(record, expected, ip):
    assert record["uuid"] == expected["id"], (record["uuid"], expected["id"])
    assert record["name"] == expected["name"]
    assert record["ip"] == ip, record["ip"]
    textures = [item for item in record["properties"] if item["name"] == "textures"]
    assert textures, "Paper did not receive texture properties"
    for texture in textures:
        assert texture.get("signature"), "Paper did not receive the texture signature"
        payload = json.loads(base64.b64decode(texture["value"], validate=True))
        assert uuid.UUID(payload["profileId"]) == uuid.UUID(expected["id"])
        assert payload["profileName"] == expected["name"]


def verify_inventory(record, item, count):
    actual = sum(value["count"] for value in record["inventory"] if value["item"] == item)
    assert actual == count, (item, actual, count)


def checkpoint(result, phase, server, directory, expected, ip, item=None, count=None):
    record = snapshot(server, directory, expected["name"])
    verify_profile(record, expected, ip)
    if item:
        verify_inventory(record, item, count)
    result["checkpoints"][phase] = record
    return record


def assert_continuity(monitor, baseline, current):
    value = op.metrics(monitor)
    assert value["connections_active"] == 1, "expected the original player connection"
    assert value["connections_accepted_total"] == baseline["connections_accepted_total"], "client reconnected during transfer"
    assert value[f'backend_players_online{{backend="{current}"}}'] == 1
    return value


def run(args, binary, directory, result):
    protocol = mc.SERVERS["paper"]["protocol"]
    primary_port, lobby_port, random_front, monitor = op.ports(4)
    front = args.port or random_front
    secret = secrets.token_hex(32)
    result.update(server="paper", version=mc.SERVERS["paper"]["version"],
                  online_mode=True, forwarding="velocity", manual_steps={}, checkpoints={},
                  ports=dict(primary=primary_port, lobby=lobby_port, proxy=front, metrics=monitor),
                  scope="online_authentication_and_velocity_forwarding", passed=False)
    plugin = build_probe(directory)
    with ExitStack() as stack:
        servers, commands, directories = {}, {}, {}
        for role, port in [("primary", primary_port), ("lobby", lobby_port)]:
            server_dir = directory / role
            server_dir.mkdir()
            command = mc.configure_server("paper", server_dir, port, role == "lobby",
                                          motd=f"rift-online-{role}", online=False)
            properties = server_dir / "server.properties"
            properties.write_text(properties.read_text().replace(
                "enforce-secure-profile=false", "enforce-secure-profile=true"))
            forwarding_config(server_dir, secret)
            (server_dir / "spigot.yml").write_text("settings:\n  bungeecord: false\n")
            plugins = server_dir / "plugins"
            plugins.mkdir()
            shutil.copyfile(plugin, plugins / plugin.name)
            servers[role] = stack.enter_context(mc.process(command, server_dir, "server.log", server=True))
            commands[role], directories[role] = command, server_dir
        for role, port in [("primary", primary_port), ("lobby", lobby_port)]:
            mc.wait_ready(servers[role], lambda p=port: mc.status_ready(p, protocol),
                          directories[role] / "server.log", timeout=600)
        result["bad_forwarding_probes"] = {
            role: reject_bad_forwarding(port, protocol)
            for role, port in [("primary", primary_port), ("lobby", lobby_port)]}
        path = directory / "rift.lua"
        path.write_text(configuration(front, monitor, primary_port, lobby_port))
        proxy = stack.enter_context(mc.process([str(binary), "--config", str(path)], directory, "proxy.log",
                                               env={**os.environ, SECRET_ENV: secret}))
        mc.wait_ready(proxy, lambda: "metrics on" in (directory / "proxy.log").read_text(), directory / "proxy.log")
        result["encryption_probe"] = encryption_challenge(front, protocol)
        print(f"\nMinecraft Java 1.21.11, signed in through your launcher. Connect to 127.0.0.1:{front}.", flush=True)
        print(f"Evidence and logs: {directory}. Backends use Velocity modern forwarding.", flush=True)
        username = args.profile or input("Your Minecraft profile name (no password/token): ").strip()
        assert re.fullmatch(r"[A-Za-z0-9_]{3,16}", username), "invalid profile name"
        expected = public_profile(username)
        result["expected_profile"] = expected
        username = expected["name"]
        # Knowing a real name/UUID still does not produce an authenticated login.
        result["claimed_identity_probe"] = encryption_challenge(front, protocol, username, uuid.UUID(expected["id"]))
        confirm(result, "initial_login",
                f"Join the lobby as {username}. Check your usual skin (F5), load chunks, move, place/break blocks, "
                "and send chat. Keep this connection open until the final disconnect instruction.")
        baseline = op.metrics(monitor)
        checkpoint(result, "lobby_initial", servers["lobby"], directories["lobby"], expected, args.expected_ip)
        console(servers["lobby"], f"give {username} minecraft:diamond 7")
        checkpoint(result, "lobby_inventory", servers["lobby"], directories["lobby"], expected, args.expected_ip,
                   "minecraft:diamond", 7)
        confirm(result, "switch_primary",
                "You received 7 diamonds in the lobby. Use /server primary without disconnecting. "
                "Check your usual skin, fresh world/chunks, block interactions and chat on primary.")
        assert_continuity(monitor, baseline, "primary")
        checkpoint(result, "primary_initial", servers["primary"], directories["primary"], expected, args.expected_ip)
        console(servers["primary"], f"give {username} minecraft:emerald 11")
        checkpoint(result, "primary_inventory", servers["primary"], directories["primary"], expected, args.expected_ip,
                   "minecraft:emerald", 11)
        confirm(result, "return_lobby_inventory",
                "You received 11 emeralds on primary. Use /hub. Verify the lobby's 7 diamonds returned, "
                "your skin is correct, and movement, blocks and chat still work. Keep the diamonds unchanged.")
        assert_continuity(monitor, baseline, "lobby")
        checkpoint(result, "lobby_return", servers["lobby"], directories["lobby"], expected, args.expected_ip,
                   "minecraft:diamond", 7)
        confirm(result, "return_primary_inventory",
                "Use /server primary again. Verify primary's 11 emeralds returned and your skin/gameplay still work. "
                "Keep the emeralds unchanged.")
        assert_continuity(monitor, baseline, "primary")
        checkpoint(result, "primary_return", servers["primary"], directories["primary"], expected, args.expected_ip,
                   "minecraft:emerald", 11)
        confirm(result, "lobby_before_bad_secret", "Use /hub and confirm you are back in the lobby with 7 diamonds.")
        assert_continuity(monitor, baseline, "lobby")
        # Only restart the unoccupied destination, keeping the user's active server alive.
        console(servers["primary"], "stop")
        assert servers["primary"].wait(timeout=45) == 0
        forwarding_config(directories["primary"], secrets.token_hex(32))
        bad_primary = stack.enter_context(mc.process(commands["primary"], directories["primary"], "wrong-secret.log", server=True))
        mc.wait_ready(bad_primary, lambda: mc.status_ready(primary_port, protocol),
                      directories["primary"] / "wrong-secret.log", timeout=600)
        primary_joins_before = sum(row["event"] == "join" for row in records(directories["primary"]))
        failures_before = op.metrics(monitor)["player_transfer_failures_total"]
        confirm(result, "bad_secret_retains_lobby",
                "Primary now has an incorrect forwarding secret. Use /server primary. Expect a clear failure message "
                "and stay in the lobby. Move, interact with blocks and chat; verify your 7 diamonds remain.")
        current = assert_continuity(monitor, baseline, "lobby")
        assert current["player_transfer_failures_total"] > failures_before
        checkpoint(result, "bad_secret_lobby_survives", servers["lobby"], directories["lobby"], expected, args.expected_ip,
                   "minecraft:diamond", 7)
        assert sum(row["event"] == "join" for row in records(directories["primary"])) == primary_joins_before, "wrong-secret login reached play"
        console(bad_primary, "stop")
        assert bad_primary.wait(timeout=45) == 0
        forwarding_config(directories["primary"], secret)
        restored = stack.enter_context(mc.process(commands["primary"], directories["primary"], "restored.log", server=True))
        mc.wait_ready(restored, lambda: mc.status_ready(primary_port, protocol),
                      directories["primary"] / "restored.log", timeout=600)
        confirm(result, "secret_repair",
                "The correct secret is restored. Use /server primary and confirm your 11 emeralds, usual skin, "
                "block interactions and chat work again, without reconnecting.")
        result["final_metrics"] = assert_continuity(monitor, baseline, "primary")
        checkpoint(result, "primary_after_repair", restored, directories["primary"], expected, args.expected_ip,
                   "minecraft:emerald", 11)
        properties = result["checkpoints"]["lobby_initial"]["properties"]
        assert all(record["properties"] == properties for record in result["checkpoints"].values()), "profile properties changed across replacement"
        confirm(result, "disconnect", "All transfer checks are complete. Disconnect normally from Minecraft.")
        op.eventually(lambda: op.metrics(monitor)["connections_active"] == 0, "client disconnected")
        for role in ("primary", "lobby"):
            assert (directories[role] / "world/playerdata" / f"{expected['id']}.dat").exists(), "playerdata did not use the Mojang UUID"
        assert proxy.poll() is None
    result["passed"] = True


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--accept-eula", action="store_true",
                        help="accept https://aka.ms/MinecraftEULA for the disposable Paper servers")
    parser.add_argument("--server", choices=["paper"], default="paper")
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--profile", help="Minecraft profile name; otherwise prompted when ready")
    parser.add_argument("--port", type=int, default=0, help="loopback frontend port (default: choose unused)")
    parser.add_argument("--expected-ip", default="127.0.0.1", help="IP Paper must receive from Rift")
    args = parser.parse_args()
    if not __debug__:
        parser.error("assertions must be enabled (do not use python -O)")
    if not args.accept_eula:
        parser.error("--accept-eula is required (https://aka.ms/MinecraftEULA)")
    if not 0 <= args.port <= 65535:
        parser.error("--port must be between 0 and 65535")
    if not shutil.which("java") or not shutil.which("javac"):
        parser.error("Java JDK 21+ must supply java and javac on PATH")
    if args.binary:
        binary = args.binary.resolve(strict=True)
    else:
        subprocess.run(["cargo", "build", "--release", "--locked"], cwd=mc.ROOT, check=True)
        binary = mc.ROOT / "target/release/rift"
    runs = mc.CACHE / "runs"
    runs.mkdir(parents=True, exist_ok=True)
    directory = Path(tempfile.mkdtemp(prefix="manual-online-", dir=runs))
    # Configuration files contain disposable forwarding secrets; reports contain
    # public profile/texture data. Keep both local to the operator by default.
    directory.chmod(0o700)
    result = {"passed": False, "logs": str(directory)}
    try:
        run(args, binary, directory, result)
    except (Exception, KeyboardInterrupt):
        result["error"] = traceback.format_exc()
        raise
    finally:
        (directory / "result.json").write_text(json.dumps(result, indent=2) + "\n")
        print(f"Manual check passed={result['passed']}; {directory / 'result.json'}", flush=True)


if __name__ == "__main__":
    main()
