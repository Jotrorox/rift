#!/usr/bin/env python3
"""Exercise the Paper evidence observer using an unsigned offline wire client.

This does not test authentication, real signatures, or downloading/rendering a
resource pack. It checks that the observer records actual Paper events and never
mislabels an unsigned message as authenticated evidence. Requires Java JDK 21+.
"""

import argparse
from datetime import datetime
import io
import os
from pathlib import Path
import shutil
import struct
import tempfile
import time
import traceback
import uuid

import manual_online as online
import minecraft as mc
from network_wire import NetworkClient, identity, read_string
import online_plugins
from operations import eventually


def packet(client, packet_id):
    for _ in range(10000):
        kind, value = client.next_packet()
        assert kind != "disconnect", value
        if kind == "packet" and value[0] == packet_id:
            return value[1]
    raise AssertionError(f"missing packet {packet_id:#x}")


def run(directory, result):
    protocol = mc.SERVERS["paper"]["protocol"]
    assert protocol == 774, "update explicit resource-pack packet IDs for the new fixture"
    port = mc.unused_port()
    plugin = online.build_probe(directory)
    command = mc.configure_server("paper", directory, port, True)
    plugins = directory / "plugins"
    result["plugin_artifacts"] = online_plugins.install(result["plugin_stack"], plugins)
    shutil.copyfile(plugin, plugins / plugin.name)
    # Paper otherwise downloads this same checksum-pinned Mojang dependency.
    (directory / "cache").mkdir()
    shutil.copyfile(mc.download("vanilla"), directory / "cache/mojang_1.21.11.jar")

    def event(kind, **fields):
        return next((row for row in reversed(online.records(directory)) if row["event"] == kind
                     and all(row.get(key) == value for key, value in fields.items())), None)

    def observed(kind, **fields):
        return eventually(lambda: event(kind, **fields), f"Paper {kind} event", timeout=15)

    with mc.process(command, directory, "server.log", server=True) as server:
        mc.wait_ready(server, lambda: mc.status_ready(port, protocol), directory / "server.log", timeout=600)
        result["plugins"] = online.plugin_checkpoint(
            server, directory, result["plugin_artifacts"]["expected_plugins"])
        # The explicit snapshot subcommand must handle reserved player names.
        name = "plugins"
        expected = {"name": name, "id": str(identity(name))}
        with NetworkClient(port, name, chat=False, protocol=protocol) as client:
            # Receiving spawn acknowledges configuration and the loaded world.
            packet(client, client.packets["position"])
            assert observed("join")["uuid"] == expected["id"]
            snapshot = online.snapshot(server, directory, name)
            assert snapshot["uuid"] == expected["id"] and snapshot["ip"] == "127.0.0.1", snapshot
            online.console(server, f"riftprobe permission {name} rift.compat.missing")
            permission = observed("permission")
            assert permission["allowed"] is False and permission["is_set"] is False, permission
            online.permission_checkpoint(result, "observer", server, directory, expected)

            # A valid signed-command packet with no signatures resolves to a
            # system message. Neither that nor ordinary unsigned chat may pass.
            marker = 'observer unsigned "quote" \\ slash ü'
            client.command("riftsigned " + marker, signed=True)
            signed = observed("signed_command")
            assert signed["message"] == signed["signed_message"] == marker, signed
            assert signed["signed"] is False and signed["signature_bytes"] == 0, signed
            client.until("message", b"Rift command recorded:")
            client.send(client.packets["chat"], mc.string(marker)
                        + struct.pack(">qq", int(time.time() * 1000), 0) + mc.empty_chat_update(protocol))
            chat = observed("chat")
            assert chat["message"] == chat["signed_message"] == marker, chat
            assert chat["signed"] is False and chat["signature_bytes"] == 0 and not chat["cancelled"], chat
            result["unsigned_message_observations"] = [signed, chat]

            # The usage response arrives after command execution, providing a
            # barrier for the negative assertion without a timing-only sleep.
            before = len(online.records(directory))
            client.command(f"riftprobe {name}")
            client.until("message", b"/riftprobe <player>")
            assert not any(row["event"] == "snapshot" for row in online.records(directory)[before:])

            statuses = ("SUCCESSFULLY_LOADED", "DECLINED", "FAILED_DOWNLOAD", "ACCEPTED",
                        "DOWNLOADED", "INVALID_URL", "FAILED_RELOAD", "DISCARDED")
            for status_id, status in enumerate(statuses):
                pack = uuid.uuid4()
                required = status_id == 0
                url, sha1 = "http://127.0.0.1:8765/pack.zip", "a" * 40
                online.console(server, f"riftprobe pack {name} {pack} {url} {sha1} {str(required).lower()}")
                request = observed("resource_pack_request", pack_id=str(pack))
                assert request["required"] is required and request["sha1"] == sha1, request
                data = io.BytesIO(packet(client, 0x4F))
                assert data.read(16) == pack.bytes
                assert read_string(data) == url and read_string(data) == sha1
                assert data.read(1) == bytes([required])
                # Synthetic responses test callback plumbing, not client loading.
                client.send(0x30, pack.bytes + mc.varint(status_id))
                response = observed("resource_pack_status", pack_id=str(pack), status=status)
                assert response["uuid"] == expected["id"], response
                online.console(server, f"riftprobe remove-pack {name} {pack}")
                observed("resource_pack_remove", pack_id=str(pack))
                assert packet(client, 0x4E) == b"\1" + pack.bytes
            result["synthetic_resource_pack_statuses"] = list(statuses)

            before = len(online.records(directory))
            for args in ("", "pack", f"pack {name}",
                         f"pack {name} {uuid.uuid4()} file:///tmp/local.zip {sha1} false",
                         f"pack {name} {uuid.uuid4()} {url} nope false",
                         f"pack {name} {uuid.uuid4()} {url} {sha1} yes",
                         f"pack {name} not-a-uuid {url} {sha1} false"):
                online.console(server, "riftprobe " + args)
            online.snapshot(server, directory, name)
            assert not any(row["event"] == "resource_pack_request" for row in online.records(directory)[before:])
            assert not event("signed_command_error")
            for row in online.records(directory):
                datetime.fromisoformat(row["utc"].replace("Z", "+00:00"))
    result["passed"] = True


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--accept-eula", action="store_true", help="accept https://aka.ms/MinecraftEULA")
    parser.add_argument("--plugin-stack", choices=online_plugins.STACKS, default="baseline")
    args = parser.parse_args()
    if not __debug__:
        parser.error("assertions must be enabled (do not use python -O)")
    if not args.accept_eula:
        parser.error("--accept-eula is required (https://aka.ms/MinecraftEULA)")
    if not shutil.which(os.environ.get("RIFT_JAVA_21", "java")) or not shutil.which("javac"):
        parser.error("Java JDK 21+ must supply java and javac")
    runs = mc.CACHE / "runs"
    runs.mkdir(parents=True, exist_ok=True)
    directory = Path(tempfile.mkdtemp(prefix="online-probe-", dir=runs))
    result = {"passed": False, "scope": "offline_observer_runtime_only", "authenticated_acceptance": "not_run",
              "plugin_stack": args.plugin_stack, "version": mc.SERVERS["paper"]["version"], "logs": str(directory)}
    print(f"Offline observer artifacts: {directory}", flush=True)
    try:
        run(directory, result)
    except (Exception, KeyboardInterrupt):
        result["error"] = traceback.format_exc()
        raise
    finally:
        online.save_result(result)
        print(f"Observer check passed={result['passed']}; {directory / 'result.json'}", flush=True)


if __name__ == "__main__":
    main()
