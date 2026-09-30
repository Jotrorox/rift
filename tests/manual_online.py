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
import online_evidence as evidence
import online_plugins
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
    save_result(result)


def save_result(result):
    path = Path(result["logs"]) / "result.json"
    pending = path.with_suffix(".tmp")
    pending.write_text(json.dumps(result, indent=2) + "\n")
    pending.replace(path)


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
    # Paper's command API references Brigadier, supplied by the pinned Mojang jar.
    with zipfile.ZipFile(mc.download("vanilla")) as vanilla:
        for entry in vanilla.infolist():
            if entry.filename.startswith("META-INF/libraries/com/mojang/brigadier/") and entry.filename.endswith(".jar"):
                (libraries / Path(entry.filename).name).write_bytes(vanilla.read(entry))
    source = mc.ROOT / "tests/fixtures/online-plugin"
    subprocess.run(["javac", "--release", "21", "-proc:none", "-classpath", str(libraries / "*"),
                    "-d", str(build), str(source / "RiftOnlineProbe.java")], check=True)
    plugin = build / "rift-online-probe.jar"
    with zipfile.ZipFile(plugin, "w") as output:
        for compiled in build.glob("RiftOnlineProbe*.class"):
            output.write(compiled, compiled.name)
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


def switch_failures(log, target):
    """Native player commands report failed transfers through structured logs."""
    failures = []
    for line in log.read_text().splitlines(keepends=True):
        if not line.startswith("{") or not line.endswith("\n"):
            continue
        event = json.loads(line)
        if event.get("event") == "backend_switch_failed" and event.get("backend") == target:
            failures.append(event)
    return failures


def snapshot(server, directory, name):
    before = len(records(directory))
    console(server, f"riftprobe snapshot {name}")
    return op.eventually(lambda: next((row for row in records(directory)[before:]
                                      if row["event"] == "snapshot" and row["name"] == name), None),
                         f"Paper profile/inventory snapshot for {name}", timeout=10)


def plugin_checkpoint(server, directory, expected):
    before = len(records(directory))
    console(server, "riftprobe plugins")
    record = op.eventually(lambda: next((row for row in records(directory)[before:]
                                        if row["event"] == "plugins"), None),
                           "enabled Paper plugin inventory", timeout=15)
    evidence.verify_plugins(record, expected)
    return record


def signed_checkpoint(result, phase, directory, expected):
    marker = f"rift_{phase}_{secrets.token_hex(4)}"
    before = len(records(directory))
    confirm(result, f"signed_{phase}",
            f"Send chat exactly: {marker}\nThen run /riftsigned {marker}\n"
            f"Then send chat exactly: {marker}_after\n"
            "Check both chat messages are visible and the signed command succeeds.")
    observations = []
    for event, message in (("chat", marker), ("signed_command", marker), ("chat", marker + "_after")):
        record = op.eventually(lambda: next((row for row in records(directory)[before:]
                                            if row["event"] == event and row.get("message") == message
                                            and row.get("uuid") == expected["id"]), None),
                               f"Paper {event} observation for {message}", timeout=10)
        evidence.verify_signed(record, expected, message, event)
        observations.append(record)
    result.setdefault("signed_checks", {})[phase] = observations
    save_result(result)


def pack_checkpoint(result, phase, server, directory, expected, pack, required=False, decline=False):
    pack_id = str(uuid.uuid4())
    before = len(records(directory))
    # The client must use Prompt in its server entry for explicit decline tests.
    print("\nResource-pack test: select " + ("No / Decline" if decline else "Yes / Accept")
          + " if prompted (Minecraft may remember your choice for this connection).", flush=True)
    console(server, f"riftprobe pack {expected['name']} {pack_id} {pack['url']} {pack['sha1']} {str(required).lower()}")
    request = op.eventually(lambda: next((row for row in records(directory)[before:]
                                         if row.get("event") == "resource_pack_request"
                                         and row.get("pack_id") == pack_id), None),
                            "Paper resource-pack request", timeout=10)
    assert request["uuid"] == expected["id"] and request["required"] is required
    assert request["url"] == pack["url"] and request["sha1"] == pack["sha1"]
    status = "DECLINED" if decline else "SUCCESSFULLY_LOADED"
    confirm(result, f"pack_{phase}",
            ("Decline the resource pack. " + ("Confirm the server disconnects you." if required
                                              else "Confirm you remain connected and can move.")) if decline else
            "Accept the resource pack, wait for it to load, and hover your inventory item: "
            f"its English name must start with RIFT {phase.split('_')[0].upper()}.")
    if decline and required:
        # Vanilla can disconnect immediately on required-pack refusal, before
        # sending a status packet. The caller must assert connection closure.
        observed = [row for row in records(directory)[before:]
                    if row.get("event") == "resource_pack_status" and row.get("pack_id") == pack_id
                    and row.get("uuid") == expected["id"]]
        assert not any(row["status"] in ("ACCEPTED", "SUCCESSFULLY_LOADED") for row in observed)
        result.setdefault("pack_checks", {})[phase] = {
            "pack_id": pack_id, "required": True, "pack": pack, "request": request,
            "observations": observed, "status_callback_required": False,
            "outcome": "operator_confirmed_refusal_disconnect"}
        save_result(result)
        return
    op.eventually(lambda: any(row.get("event") == "resource_pack_status" and row.get("pack_id") == pack_id
                             and row.get("uuid") == expected["id"] and row.get("status") == status
                             for row in records(directory)[before:]), "Paper resource-pack response", timeout=10)
    result.setdefault("pack_checks", {})[phase] = {
        "pack_id": pack_id, "required": required, "pack": pack, "request": request,
        "observations": evidence.verify_pack_status(records(directory)[before:], expected, pack_id, status)}
    save_result(result)


def permission_checkpoint(result, phase, server, directory, expected):
    """Prove a UUID-based LuckPerms grant/revoke reaches Paper's permission API."""
    if result["plugin_stack"] != "essentials":
        return
    node = "rift.acceptance." + secrets.token_hex(4)
    checks = []
    for allowed in (True, False):
        console(server, f"lp user {expected['id']} permission set {node} {str(allowed).lower()}")

        def observe():
            before = len(records(directory))
            console(server, f"riftprobe permission {expected['name']} {node}")
            record = op.eventually(lambda: next((row for row in records(directory)[before:]
                                                if row["event"] == "permission" and row.get("permission") == node
                                                and row.get("uuid") == expected["id"]), None),
                                   "Paper permission observation", timeout=5)
            return record if record["allowed"] is allowed and record["is_set"] is True else None

        checks.append(op.eventually(observe, "LuckPerms permission update", timeout=15))
    console(server, f"lp user {expected['id']} permission unset {node}")
    result.setdefault("permission_checks", {})[phase] = checks
    save_result(result)


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
                  scope="paper_plugin_preflight" if args.preflight else "authenticated_paper_compatibility",
                  plugin_stack=args.plugin_stack, authenticated_acceptance="not_run", passed=False,
                  server_fixture=mc.SERVERS["paper"],
                  binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest())
    save_result(result)
    plugin = build_probe(directory)
    with ExitStack() as stack:
        packs = stack.enter_context(evidence.resource_packs(directory, args.pack_port))
        result["resource_packs"] = packs
        servers, commands, directories = {}, {}, {}
        result["plugin_artifacts"], result["plugin_observations"] = {}, {}
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
            result["plugin_artifacts"][role] = online_plugins.install(args.plugin_stack, plugins)
            assert result["plugin_artifacts"][role]["paper_version"] == mc.SERVERS["paper"]["version"], "plugin matrix targets another Paper fixture"
            servers[role] = stack.enter_context(mc.process(command, server_dir, "server.log", server=True))
            commands[role], directories[role] = command, server_dir
        for role, port in [("primary", primary_port), ("lobby", lobby_port)]:
            mc.wait_ready(servers[role], lambda p=port: mc.status_ready(p, protocol),
                          directories[role] / "server.log", timeout=600)
            result["plugin_observations"][role] = plugin_checkpoint(
                servers[role], directories[role], result["plugin_artifacts"][role]["expected_plugins"])
        result["bad_forwarding_probes"] = {
            role: reject_bad_forwarding(port, protocol)
            for role, port in [("primary", primary_port), ("lobby", lobby_port)]}
        path = directory / "rift.lua"
        path.write_text(configuration(front, monitor, primary_port, lobby_port))
        proxy = stack.enter_context(mc.process([str(binary), "--config", str(path)], directory, "proxy.log",
                                               env={**os.environ, SECRET_ENV: secret}))
        mc.wait_ready(proxy, lambda: "metrics on" in (directory / "proxy.log").read_text(), directory / "proxy.log")
        result["encryption_probe"] = encryption_challenge(front, protocol)
        save_result(result)
        if args.preflight:
            # No licensed account has authenticated. Never report online acceptance.
            result["passed"] = True
            return
        print(f"\nMinecraft Java 1.21.11, signed in through your launcher. Connect to 127.0.0.1:{front}.", flush=True)
        print(f"Evidence and logs: {directory}. Backends use Velocity modern forwarding.", flush=True)
        print("Use English (US) and set Server Resource Packs to Prompt in the server entry. "
              f"For SSH, also forward resource-pack port {packs['lobby']['url'].split(':')[2].split('/')[0]}.", flush=True)
        username = args.profile or input("Your Minecraft profile name (no password/token): ").strip()
        assert re.fullmatch(r"[A-Za-z0-9_]{3,16}", username), "invalid profile name"
        expected = public_profile(username)
        result["expected_profile"] = expected
        result["authenticated_acceptance"] = "in_progress"
        username = expected["name"]
        # Knowing a real name/UUID still does not produce an authenticated login.
        result["claimed_identity_probe"] = encryption_challenge(front, protocol, username, uuid.UUID(expected["id"]))
        confirm(result, "initial_login",
                f"Join the lobby as {username}. Check your usual skin (F5), load chunks, move, place/break blocks, "
                "and send chat. Keep this connection open until the final disconnect instruction.")
        baseline = op.metrics(monitor)
        result["initial_metrics"] = baseline
        checkpoint(result, "lobby_initial", servers["lobby"], directories["lobby"], expected, args.expected_ip)
        console(servers["lobby"], f"minecraft:give {username} minecraft:diamond 7")
        checkpoint(result, "lobby_inventory", servers["lobby"], directories["lobby"], expected, args.expected_ip,
                   "minecraft:diamond", 7)
        signed_checkpoint(result, "lobby_initial", directories["lobby"], expected)
        permission_checkpoint(result, "lobby_initial", servers["lobby"], directories["lobby"], expected)
        pack_checkpoint(result, "lobby_loaded", servers["lobby"], directories["lobby"], expected, packs["lobby"])
        confirm(result, "switch_primary",
                "You received 7 diamonds in the lobby. Use /server primary without disconnecting. "
                "Check your usual skin, fresh world/chunks, block interactions and chat on primary. "
                "In the creative inventory, diamonds and emeralds must have their normal English names "
                "(the lobby resource pack must be removed).")
        assert_continuity(monitor, baseline, "primary")
        checkpoint(result, "primary_initial", servers["primary"], directories["primary"], expected, args.expected_ip)
        console(servers["primary"], f"minecraft:give {username} minecraft:emerald 11")
        checkpoint(result, "primary_inventory", servers["primary"], directories["primary"], expected, args.expected_ip,
                   "minecraft:emerald", 11)
        signed_checkpoint(result, "primary_initial", directories["primary"], expected)
        permission_checkpoint(result, "primary_initial", servers["primary"], directories["primary"], expected)
        pack_checkpoint(result, "primary_loaded", servers["primary"], directories["primary"], expected,
                        packs["primary"], required=True)
        confirm(result, "return_lobby_inventory",
                "You received 11 emeralds on primary. Use /hub. Verify the lobby's 7 diamonds returned, "
                "your skin is correct, and movement, blocks and chat still work. Keep the diamonds unchanged. "
                "The item names must be normal again: primary's pack must be removed.")
        assert_continuity(monitor, baseline, "lobby")
        checkpoint(result, "lobby_return", servers["lobby"], directories["lobby"], expected, args.expected_ip,
                   "minecraft:diamond", 7)
        signed_checkpoint(result, "lobby_return", directories["lobby"], expected)
        confirm(result, "return_primary_inventory",
                "Use /server primary again. Verify primary's 11 emeralds returned and your skin/gameplay still work. "
                "Keep the emeralds unchanged.")
        assert_continuity(monitor, baseline, "primary")
        checkpoint(result, "primary_return", servers["primary"], directories["primary"], expected, args.expected_ip,
                   "minecraft:emerald", 11)
        signed_checkpoint(result, "primary_return", directories["primary"], expected)
        confirm(result, "lobby_before_bad_secret", "Use /hub and confirm you are back in the lobby with 7 diamonds.")
        assert_continuity(monitor, baseline, "lobby")
        pack_checkpoint(result, "lobby_retained", servers["lobby"], directories["lobby"], expected, packs["lobby"])
        # Only restart the unoccupied destination, keeping the user's active server alive.
        console(servers["primary"], "stop")
        assert servers["primary"].wait(timeout=45) == 0
        forwarding_config(directories["primary"], secrets.token_hex(32))
        bad_primary = stack.enter_context(mc.process(commands["primary"], directories["primary"], "wrong-secret.log", server=True))
        mc.wait_ready(bad_primary, lambda: mc.status_ready(primary_port, protocol),
                      directories["primary"] / "wrong-secret.log", timeout=600)
        result["plugin_observations"]["primary_wrong_secret"] = plugin_checkpoint(
            bad_primary, directories["primary"], result["plugin_artifacts"]["primary"]["expected_plugins"])
        primary_joins_before = sum(row["event"] == "join" for row in records(directories["primary"]))
        failures_before = len(switch_failures(directory / "proxy.log", "primary"))
        confirm(result, "bad_secret_retains_lobby",
                "Primary now has an incorrect forwarding secret. Use /server primary. Expect a clear failure message "
                "and stay in the lobby. Move, interact with blocks and chat; verify your 7 diamonds remain "
                "and their name still starts with RIFT LOBBY (the active pack must survive the failed transfer).")
        assert_continuity(monitor, baseline, "lobby")
        failures = switch_failures(directory / "proxy.log", "primary")
        assert len(failures) > failures_before, "Rift did not record a rejected transfer to primary"
        rejection = failures[-1]
        assert rejection["error_kind"] == "PermissionDenied", rejection
        assert "Unable to verify player details" in rejection["message"], rejection
        result["wrong_secret_rejection"] = rejection
        checkpoint(result, "bad_secret_lobby_survives", servers["lobby"], directories["lobby"], expected, args.expected_ip,
                   "minecraft:diamond", 7)
        signed_checkpoint(result, "bad_secret_lobby", directories["lobby"], expected)
        permission_checkpoint(result, "bad_secret_lobby", servers["lobby"], directories["lobby"], expected)
        assert sum(row["event"] == "join" for row in records(directories["primary"])) == primary_joins_before, "wrong-secret login reached play"
        console(bad_primary, "stop")
        assert bad_primary.wait(timeout=45) == 0
        forwarding_config(directories["primary"], secret)
        restored = stack.enter_context(mc.process(commands["primary"], directories["primary"], "restored.log", server=True))
        mc.wait_ready(restored, lambda: mc.status_ready(primary_port, protocol),
                      directories["primary"] / "restored.log", timeout=600)
        result["plugin_observations"]["primary_restored"] = plugin_checkpoint(
            restored, directories["primary"], result["plugin_artifacts"]["primary"]["expected_plugins"])
        confirm(result, "secret_repair",
                "The correct secret is restored. Use /server primary and confirm your 11 emeralds, usual skin, "
                "block interactions and chat work again, without reconnecting. "
                "Inventory item names must be normal again (the lobby pack is removed).")
        result["final_metrics"] = assert_continuity(monitor, baseline, "primary")
        checkpoint(result, "primary_after_repair", restored, directories["primary"], expected, args.expected_ip,
                   "minecraft:emerald", 11)
        signed_checkpoint(result, "primary_repaired", directories["primary"], expected)
        permission_checkpoint(result, "primary_repaired", restored, directories["primary"], expected)
        properties = result["checkpoints"]["lobby_initial"]["properties"]
        assert all(record["properties"] == properties for record in result["checkpoints"].values()), "profile properties changed across replacement"
        confirm(result, "disconnect", "All transfer checks are complete. Disconnect normally from Minecraft.")
        op.eventually(lambda: op.metrics(monitor)["connections_active"] == 0, "client disconnected")
        for role in ("primary", "lobby"):
            assert (directories[role] / "world/playerdata" / f"{expected['id']}.dat").exists(), "playerdata did not use the Mojang UUID"
        # Vanilla remembers pack consent for the connection. Test refusal on a
        # separate authenticated connection, after all continuity assertions.
        confirm(result, "decline_login",
                "In the server-list entry set Server Resource Packs back to Prompt, then join the lobby again. "
                "This separate connection tests refusal; confirm your 7 diamonds and gameplay work.")
        decline_baseline = op.metrics(monitor)
        assert decline_baseline["connections_active"] == 1
        checkpoint(result, "lobby_decline_login", servers["lobby"], directories["lobby"], expected,
                   args.expected_ip, "minecraft:diamond", 7)
        pack_checkpoint(result, "lobby_optional_declined", servers["lobby"], directories["lobby"], expected,
                        packs["lobby"], decline=True)
        assert_continuity(monitor, decline_baseline, "lobby")
        signed_checkpoint(result, "optional_decline", directories["lobby"], expected)
        pack_checkpoint(result, "lobby_required_declined", servers["lobby"], directories["lobby"], expected,
                        packs["lobby"], required=True, decline=True)
        op.eventually(lambda: op.metrics(monitor)["connections_active"] == 0, "required pack refusal disconnected client")
        result["decline_metrics"] = op.metrics(monitor)
        assert result["decline_metrics"]["connections_accepted_total"] == decline_baseline["connections_accepted_total"]
        assert proxy.poll() is None
    result["authenticated_acceptance"] = "passed"
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
    parser.add_argument("--plugin-stack", choices=online_plugins.STACKS, default="baseline")
    parser.add_argument("--preflight", action="store_true",
                        help="check real Paper/plugin startup and rejection probes; no authenticated client acceptance")
    parser.add_argument("--pack-port", type=int, default=0, help="loopback HTTP port for resource packs (SSH forward this too)")
    args = parser.parse_args()
    if not __debug__:
        parser.error("assertions must be enabled (do not use python -O)")
    if not args.accept_eula:
        parser.error("--accept-eula is required (https://aka.ms/MinecraftEULA)")
    if not 0 <= args.port <= 65535 or not 0 <= args.pack_port <= 65535:
        parser.error("--port and --pack-port must be between 0 and 65535")
    if args.port and args.port == args.pack_port:
        parser.error("--port and --pack-port must differ")
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
        result["passed"] = False
        if result.get("authenticated_acceptance") == "in_progress":
            result["authenticated_acceptance"] = "failed"
        raise
    finally:
        save_result(result)
        print(f"{result.get('scope', 'setup')} passed={result['passed']}; "
              f"authenticated_acceptance={result.get('authenticated_acceptance', 'not_run')}; "
              f"{directory / 'result.json'}", flush=True)


if __name__ == "__main__":
    main()
