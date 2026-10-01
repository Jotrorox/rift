#!/usr/bin/env python3
"""Template provisioning and storage policies through the live admin/HTTP APIs."""

import argparse
from contextlib import closing
import json
import os
from pathlib import Path
import socket
import sys
import tempfile

from managed_wire import SECRET, admin, eventually
from minecraft import ROOT, process, unused_port, wait_ready
from network_wire import NetworkClient
from services_wire import api, available_range


def check(binary):
    with tempfile.TemporaryDirectory(prefix="rift-provisioning-wire-") as temporary:
        directory = Path(temporary)
        assets = directory / "assets"
        (assets / "configs/plugins/Example").mkdir(parents=True)
        (assets / "map/region").mkdir(parents=True)
        (assets / "paper.jar").write_bytes(b"server fixture")
        (assets / "Example.jar").write_bytes(b"plugin fixture")
        (assets / "configs/plugins/Example/config.yml").write_text("enabled: true\n")
        (assets / "configs/server.properties").write_text(
            "motd=Template arena\nserver-port=1\nserver-ip=0.0.0.0\nlevel-name=shared\n"
        )
        (assets / "map/region/r.0.0.mca").write_bytes(b"original world")
        first = available_range()
        allocated = set(range(first, first + 5))

        def port():
            while True:
                candidate = unused_port()
                if candidate not in allocated:
                    allocated.add(candidate)
                    return candidate

        front, control, dashboard = port(), port(), port()
        argv = ",".join(json.dumps(value) for value in [
            sys.executable, str(Path(__file__).with_name("services_wire.py").resolve()),
            "--child", "{port}", "--name", "{name}",
        ])
        config = directory / "rift.lua"
        source = f"""return {{
            listeners={{public='127.0.0.1:{front}'}},backends={{}},routes={{public='games'}},
            network={{hubs={{'games','survival'}}}},shutdown_timeout_ms=1000,
            admin={{listen='127.0.0.1:{control}',permissions={{'servers','reload','shutdown'}}}},
            web={{listen='127.0.0.1:{dashboard}',token='{SECRET}'}},
            templates={{arena={{server_jar='assets/paper.jar',plugins={{'assets/Example.jar'}},
                configs='assets/configs',map='assets/map'}},broken={{server_jar='assets/missing.jar'}}}},
            service_groups={{
                games={{command={{{argv}}},directory='instances/{{name}}',template='arena',
                    storage='disposable',port_range={{{first},{first+4}}},start_timeout_ms=5000,stop_timeout_ms=1000}},
                survival={{command={{{argv}}},directory='worlds/{{name}}',template='arena',
                    storage='persistent',port_range={{{first},{first+4}}},start_timeout_ms=5000,stop_timeout_ms=1000}},
                broken={{command={{{argv}}},directory='instances/{{name}}',template='broken',
                    storage='disposable',port_range={{{first},{first+4}}}}}
            }}
        }}"""
        config.write_text(source)
        environment = dict(os.environ, RIFT_ADMIN_TOKEN=SECRET)
        # Validation neither reads missing assets nor creates instance directories.
        import subprocess
        validation = subprocess.run([str(binary), "check", str(config)],
                                    stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        assert validation.returncode == 0, validation.stderr.decode()
        assert not (directory / "instances").exists()

        def operation(*args):
            result = admin(control, *args)
            assert result["ok"], result
            return result["data"]

        def servers():
            return {value["name"]: value for value in operation("servers")["servers"]}

        def start_proxy(log_name):
            return process([str(binary), "--config", str(config)], directory,
                           log_name, env=environment)

        with start_proxy("first.log") as proxy:
            wait_ready(proxy, lambda: "rift: admin on" in (directory / "first.log").read_text(),
                       directory / "first.log", timeout=10)
            groups = {value["name"]: value for value in operation("groups")["groups"]}
            assert groups["games"]["storage"] == "disposable"
            assert groups["survival"]["storage"] == "persistent"
            assert groups["games"]["template"] == "arena"
            failure, _ = api(dashboard, "POST", "/api/groups/broken/instances")
            assert failure == 400
            assert servers() == {}
            assert not (directory / "instances/broken-1").exists()
            (assets / "missing.jar").write_bytes(b"fixed fixture")
            fixed = operation("create", "broken")
            assert fixed["port"] == first  # Failed copy released the reservation.
            fixed_dir = directory / "instances" / fixed["name"]
            marker_path = fixed_dir / ".rift-instance.json"
            original_marker = marker_path.read_bytes()
            marker_path.write_text("{}")
            assert api(dashboard, "DELETE", f"/api/instances/{fixed['name']}")[0] == 409
            assert fixed["name"] in servers() and fixed_dir.exists()
            marker_path.write_bytes(original_marker)
            assert operation("remove", fixed["name"])["files_removed"] is True
            if os.name == "posix" and os.geteuid() != 0:
                # A post-reaping filesystem error must still publish deregistration.
                cleanup_fixture = operation("create", "broken")
                cleanup_dir = directory / "instances" / cleanup_fixture["name"]
                cleanup_dir.chmod(0o555)
                try:
                    removed = operation("remove", cleanup_fixture["name"])
                    assert removed["removed"] and removed["cleanup_error"], removed
                    assert removed["files_removed"] is False
                    assert cleanup_fixture["name"] not in servers()
                finally:
                    cleanup_dir.chmod(0o755)
                import shutil
                shutil.rmtree(cleanup_dir)
            code, game = api(dashboard, "POST", "/api/groups/games/instances")
            assert code == 201 and game["storage"] == "disposable", game
            persistent = operation("create", "survival")
            game_dir = directory / "instances/games-1"
            world_dir = directory / "worlds/survival-1"
            for target, instance in [(game_dir, game), (world_dir, persistent)]:
                assert (target / "server.jar").read_bytes() == b"server fixture"
                assert (target / "plugins/Example.jar").read_bytes() == b"plugin fixture"
                assert (target / "plugins/Example/config.yml").read_text() == "enabled: true\n"
                assert (target / "world/region/r.0.0.mca").read_bytes() == b"original world"
                properties = (target / "server.properties").read_text()
                assert f"server-port={instance['port']}" in properties
                assert "server-ip=127.0.0.1" in properties and "level-name=world" in properties
                assert "motd=Template arena" in properties
                assert not (target / "eula.txt").exists()
            assert servers()["games-1"]["template"] == "arena"
            status_code, status = api(dashboard, "GET", "/api/status")
            assert status_code == 200
            assert {value["storage"] for value in status["service_groups"]} == {"persistent", "disposable"}
            # A connected player protects both the process and its disposable files.
            with NetworkClient(front, "Alice") as client:
                client.joined("games-1")
                client.probe("games-1")
                assert api(dashboard, "DELETE", "/api/instances/games-1")[0] == 409
                assert game_dir.exists()
            eventually(lambda: servers()["games-1"]["reservations"] == 0, "attachment released")
            operation("stop", "games-1")
            eventually(lambda: servers()["games-1"]["state"] == "stopped", "game stopped")
            assert game_dir.exists()  # Stopping never resets an active game.
            operation("start", "games-1")
            eventually(lambda: servers()["games-1"]["state"] == "running", "game restarted")
            code, removed = api(dashboard, "DELETE", "/api/instances/games-1")
            assert code == 200 and removed["files_removed"] is True, removed
            assert not game_dir.exists()
            with closing(socket.socket()) as probe:
                assert probe.connect_ex(("127.0.0.1", game["port"])) != 0
            (world_dir / "world/region/r.0.0.mca").write_bytes(b"player progress")
            removed = operation("remove", "survival-1")
            assert removed["files_removed"] is False and world_dir.exists()
            # Live template edits affect future provisioning and preserve existing files.
            operation("reload")
            existing = operation("create", "games")
            existing_dir = directory / "instances" / existing["name"]
            (assets / "other.jar").write_bytes(b"updated server fixture")
            config.write_text(source.replace("assets/paper.jar", "assets/other.jar"))
            operation("reload")
            updated = operation("create", "games")
            updated_dir = directory / "instances" / updated["name"]
            assert (updated_dir / "server.jar").read_bytes() == b"updated server fixture"
            assert (existing_dir / "server.jar").read_bytes() == b"server fixture"
            assert existing["name"] in servers()
            assert (world_dir / "server.jar").read_bytes() == b"server fixture"
            assert (world_dir / "world/region/r.0.0.mca").read_bytes() == b"player progress"
            operation("remove", existing["name"])
            operation("remove", updated["name"])
            config.write_text(source)
            operation("reload")
            operation("shutdown")
            proxy.wait(timeout=8)
            assert proxy.returncode == 0
        # Runtime registrations reset; persistent files reattach without reseeding.
        with start_proxy("second.log") as proxy:
            wait_ready(proxy, lambda: "rift: admin on" in (directory / "second.log").read_text(),
                       directory / "second.log", timeout=10)
            assert servers() == {}
            with closing(socket.socket()) as occupied:
                occupied.bind(("127.0.0.1", persistent["port"]))
                occupied.listen()
                reattached = operation("create", "survival")
                assert reattached["name"] == "survival-1" and reattached["port"] != persistent["port"]
            assert (world_dir / "world/region/r.0.0.mca").read_bytes() == b"player progress"
            assert f"server-port={reattached['port']}" in (world_dir / "server.properties").read_text()
            # Every disposable recreation starts from the unchanged map asset.
            recreated = operation("create", "games")
            assert recreated["name"] == "games-1"
            assert (game_dir / "world/region/r.0.0.mca").read_bytes() == b"original world"
            operation("remove", "games-1")
            operation("remove", "survival-1")
            operation("shutdown")
            proxy.wait(timeout=8)
            assert proxy.returncode == 0
        assert (assets / "map/region/r.0.0.mca").read_bytes() == b"original world"
    print("PASS: template assets, copy rollback, port release, storage metadata, occupied removal, "
          "stop/restart, disposable cleanup, persistent reattachment and live template reload")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/rift")
    args = parser.parse_args()
    if not __debug__:
        parser.error("assertions must be enabled")
    check(args.binary.resolve())
