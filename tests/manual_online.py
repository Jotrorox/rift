#!/usr/bin/env python3
"""Interactive, authenticated gameplay check. Requires a signed-in Java client.

This is deliberately separate from CI: no Microsoft credentials or tokens are
accepted by the harness. The operator signs in using their Minecraft launcher.
"""

import argparse
from contextlib import ExitStack
import hashlib
import io
import json
from pathlib import Path
import platform
import re
import signal
import subprocess
import tempfile
import time
import traceback
import uuid

import minecraft as mc
import operations as op


def encryption_challenge(port, protocol):
    """An unauthenticated probe must get Encryption Request, never Login Success."""
    with mc.Client(port, 2, protocol) as client:
        client.send(0, mc.string("RiftAuthProbe") + uuid.uuid4().bytes)
        packet_id, body = client.receive()
        assert packet_id == 1, f"expected Encryption Request, received {packet_id}"
        data = io.BytesIO(body)
        data.read(mc.read_varint(data))  # Server ID.
        key = data.read(mc.read_varint(data))
        challenge = data.read(mc.read_varint(data))
        assert len(key) >= 128 and len(challenge) >= 4, "missing encryption key/challenge"
        assert data.read() == b"\x01", "server did not require authentication"
    return {"encryption_request": True, "should_authenticate": True}


def confirm(result, step, instruction):
    print(f"\n{instruction}", flush=True)
    if input("Type PASS only after verifying this in the game (anything else fails): ").strip() != "PASS":
        raise AssertionError(f"operator did not confirm {step}")
    result["manual_steps"][step] = {"passed": True, "utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())}


def authenticated_profile(log, username):
    text = log.read_text(errors="replace")
    match = re.search(rf"UUID of player {re.escape(username)} is ([0-9a-f-]{{36}})", text)
    assert match, "server log has no authenticated profile UUID for this player"
    player_id = uuid.UUID(match[1])
    offline_id = uuid.UUID(bytes=hashlib.md5(f"OfflinePlayer:{username}".encode()).digest(), version=3)
    assert player_id != offline_id, "server used an offline UUID"
    assert re.search(rf"{re.escape(username)}\[.*logged in with entity id", text), "login did not complete"
    return str(player_id)


def run(args, binary, directory, result):
    protocol = mc.SERVERS[args.server]["protocol"]
    primary_port, lobby_port, front, monitor = op.ports(4)
    result.update(server=args.server, version=mc.SERVERS[args.server]["version"],
                  compression=True, online_mode=True, manual_steps={}, passed=False,
                  scope="recovery_and_drain" if args.recovery_only else "full")
    with ExitStack() as stack:
        servers, commands, dirs = {}, {}, {}
        for role, port in [("primary", primary_port), ("lobby", lobby_port)]:
            server_dir = directory / role
            server_dir.mkdir()
            command = mc.configure_server(args.server, server_dir, port, True,
                                          motd=f"rift-online-{role}", online=True)
            servers[role] = stack.enter_context(mc.process(command, server_dir, "server.log", server=True))
            commands[role], dirs[role] = command, server_dir
        for role, port in [("primary", primary_port), ("lobby", lobby_port)]:
            mc.wait_ready(servers[role], lambda p=port: mc.status_ready(p, protocol), dirs[role] / "server.log")
        path = directory / "rift.lua"
        source = op.configuration(front, monitor, primary_port, lobby_port, shutdown_ms=120000)
        initial_role = "lobby" if args.recovery_only else "primary"
        op.write_config(path, op.configuration(front, monitor, primary_port, lobby_port,
                                               target=initial_role, shutdown_ms=120000))
        proxy = stack.enter_context(mc.process([str(binary), "--config", str(path)], directory, "proxy.log"))
        mc.wait_ready(proxy, lambda: "metrics on" in (directory / "proxy.log").read_text(), directory / "proxy.log")
        resources = stack.enter_context(op.Resources(proxy, directory))
        result["encryption_probes"] = {role: encryption_challenge(port, protocol)
                                       for role, port in [("primary", primary_port), ("lobby", lobby_port), ("rift", front)]}
        print(f"\nMinecraft Java {mc.SERVERS[args.server]['version'].split('-')[0]}, signed in through your launcher.", flush=True)
        print(f"Connect to 127.0.0.1:{front}. Both backends require online authentication.", flush=True)
        print(f"Logs and results: {directory}", flush=True)
        username = input("Your Minecraft profile name (no password/token): ").strip()
        assert re.fullmatch(r"[A-Za-z0-9_]{3,16}", username), "invalid profile name"
        confirm(result, "authenticated_gameplay",
                f"Join the {initial_role}, load chunks, place/break a block and send chat. "
                "Do not reconnect again until explicitly instructed.")
        result["profile_uuid"] = authenticated_profile(dirs[initial_role] / "server.log", username)
        login_pattern = rf"{re.escape(username)}\[.*logged in with entity id"
        if not args.recovery_only:
            # Setup may include deliberate reconnects. Check continuity against the
            # login count at the start of stress, preserving all earlier log evidence.
            initial_logins = len(re.findall(login_pattern, (dirs["primary"] / "server.log").read_text()))
            result["logins_before_reload_stress"] = initial_logins
            before = op.metrics(monitor)
            baseline = resources.sample()
            quiet = []
            sample_start = len(resources.samples)
            for index in range(12):
                target = "lobby" if index % 2 == 0 else "primary"
                candidate = op.configuration(front, monitor, primary_port, lobby_port,
                                             target=target, shutdown_ms=120000)
                op.reload_config(proxy, monitor, path, candidate)
                expected = mc.status(lobby_port if target == "lobby" else primary_port, protocol)["description"]
                op.burst(front, protocol, expected)
                op.burst(front, protocol, expected, unique=True)
                if index % 3 == 0:
                    op.reload_config(proxy, monitor, path, "return {", valid=False)
                op.eventually(lambda: op.metrics(monitor)["connections_active"] == 1, "one authenticated player remains")
                quiet.append(resources.sample())
                time.sleep(0.5)
            confirm(result, "reloads_and_bursts",
                    "Verify you stayed connected through all reloads and status bursts. Move, load chunks, place/break blocks and chat again.")
            after = op.metrics(monitor)
            assert after["client_bytes_read_total"] > before["client_bytes_read_total"]
            # Establish that this was one login rather than an automatic reconnect.
            assert len(re.findall(login_pattern, (dirs["primary"] / "server.log").read_text())) == initial_logins
            assert not re.search(login_pattern, (dirs["lobby"] / "server.log").read_text())
            op.assert_resources(baseline, quiet, resources.samples[sample_start:])
            result["resources"] = dict(baseline=baseline, quiet=quiet,
                                       peak={key: max(s[key] for s in resources.samples[sample_start:])
                                             for key in ["rss_bytes", "fds", "sockets"]})

            # Move the human player to the lobby before stopping the primary: an
            # outage cannot preserve a session whose own backend is stopped.
            op.reload_config(proxy, monitor, path, op.configuration(
                front, monitor, primary_port, lobby_port, target="lobby", shutdown_ms=120000))
            confirm(result, "lobby_login", "Disconnect once, then reconnect to the same address. Verify gameplay in the lobby.")
            assert authenticated_profile(dirs["lobby"] / "server.log", username) == result["profile_uuid"]
            op.reload_config(proxy, monitor, path, source)
            op.stop_server(servers["primary"])
            op.eventually(lambda: op.metrics(monitor)['backend_up{backend="primary"}'] == 0, "primary down")
            time.sleep(1.1)
            op.burst(front, protocol, mc.status(lobby_port, protocol)["description"])
            confirm(result, "outage_survival", "Keep playing in the lobby. Verify the primary outage and status burst did not disconnect you.")
            confirm(result, "authenticated_fallback", "Disconnect and reconnect once while the primary is down. Verify authenticated gameplay still reaches the lobby.")
            assert len(re.findall(login_pattern, (dirs["lobby"] / "server.log").read_text())) == 2
            assert op.metrics(monitor)["fallbacks_total"] > 0
        else:
            # Repeat only the interrupted continuity/drain checks in a fresh
            # fixture; the scoped report cannot stand in for the earlier phases.
            op.reload_config(proxy, monitor, path, source)
            op.stop_server(servers["primary"])
            op.eventually(lambda: op.metrics(monitor)['backend_up{backend="primary"}'] == 0, "primary down")
        lobby_logins_before_recovery = len(re.findall(login_pattern, (dirs["lobby"] / "server.log").read_text()))
        recovered = stack.enter_context(mc.process(commands["primary"], dirs["primary"], "restarted.log", server=True))
        mc.wait_ready(recovered, lambda: mc.status_ready(primary_port, protocol), dirs["primary"] / "restarted.log")
        op.eventually(lambda: op.metrics(monitor)['backend_up{backend="primary"}'] == 1, "primary recovered")
        confirm(result, "recovery_survival", "Keep playing without reconnecting. Verify primary recovery leaves your lobby session intact.")
        assert not re.search(login_pattern, (dirs["primary"] / "restarted.log").read_text())
        assert len(re.findall(login_pattern, (dirs["lobby"] / "server.log").read_text())) == lobby_logins_before_recovery
        op.eventually(lambda: op.metrics(monitor)["connections_active"] == 1, "one player before drain")
        before = op.metrics(monitor)
        proxy.send_signal(signal.SIGTERM)
        op.eventually(lambda: op.listener_closed(front), "listener closes during drain")
        confirm(result, "encrypted_drain",
                "Within 120 seconds: keep moving, place/break a block and send chat. Verify gameplay continues during drain; stay connected until typing PASS.")
        after = op.metrics(monitor)
        assert after["connections_active"] == 1 and after["forced_shutdowns_total"] == 0
        for key in ["client_bytes_read_total", "client_bytes_written_total"]:
            assert after[key] > before[key], f"no live traffic during drain: {key}"
        result["metrics_during_drain"] = after
        confirm(result, "graceful_exit", "Now disconnect normally from Minecraft.")
        assert proxy.wait(timeout=10) == 0
        assert "shutdown deadline reached" not in (directory / "proxy.log").read_text()
    result["passed"] = True


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--accept-eula", action="store_true")
    parser.add_argument("--server", choices=["vanilla", "paper"], default="paper")
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--recovery-only", action="store_true",
                        help="repeat only authenticated recovery continuity and drain in a fresh fixture")
    args = parser.parse_args()
    if not __debug__ or platform.system() != "Linux":
        parser.error("requires Linux and enabled Python assertions (no -O)")
    if not args.accept_eula:
        parser.error("--accept-eula is required (https://aka.ms/MinecraftEULA)")
    if args.binary:
        binary = args.binary.resolve(strict=True)
    else:
        subprocess.run(["cargo", "build", "--release", "--locked"], cwd=mc.ROOT, check=True)
        binary = mc.ROOT / "target/release/rift"
    runs = mc.CACHE / "runs"
    runs.mkdir(parents=True, exist_ok=True)
    directory = Path(tempfile.mkdtemp(prefix="manual-online-", dir=runs))
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
