"""Evidence checks and disposable resource packs for the real online fixture."""

from contextlib import contextmanager
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import io
import json
import threading
import uuid
import zipfile


def pack_bytes(role):
    """A deterministic language-only pack: visible item names, no external assets.

    Minecraft 1.21.11's pinned version.json declares resource format 75.0.
    """
    if role not in ("lobby", "primary"):
        raise ValueError("unknown resource-pack role")
    output = io.BytesIO()
    files = {
        "pack.mcmeta": {"pack": {"description": f"Rift {role} acceptance pack",
                                  "min_format": [75, 0], "max_format": [75, 0]}},
        "assets/minecraft/lang/en_us.json": {
            "item.minecraft.diamond": f"RIFT {role.upper()} Diamond",
            "item.minecraft.emerald": f"RIFT {role.upper()} Emerald"},
    }
    with zipfile.ZipFile(output, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        for name, value in files.items():
            entry = zipfile.ZipInfo(name, date_time=(2026, 1, 1, 0, 0, 0))
            entry.compress_type = zipfile.ZIP_DEFLATED
            archive.writestr(entry, json.dumps(value, sort_keys=True).encode())
    return output.getvalue()


@contextmanager
def resource_packs(directory, port=0):
    """Serve only two generated zips; never expose the run directory/secrets."""
    payloads = {f"/{role}.zip": pack_bytes(role) for role in ("lobby", "primary")}

    class Handler(BaseHTTPRequestHandler):
        def do_GET(self):
            payload = payloads.get(self.path)
            if payload is None:
                self.send_error(404)
                return
            self.send_response(200)
            self.send_header("Content-Type", "application/zip")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

        def log_message(self, *_args):
            pass

    server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        packs = {}
        for role in ("lobby", "primary"):
            payload = payloads[f"/{role}.zip"]
            (directory / f"{role}.zip").write_bytes(payload)
            packs[role] = {"url": f"http://127.0.0.1:{server.server_port}/{role}.zip",
                           "sha1": hashlib.sha1(payload).hexdigest(),
                           "sha256": hashlib.sha256(payload).hexdigest()}
        yield packs
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


def verify_plugins(record, expected):
    assert record["event"] == "plugins"
    actual = {plugin["name"]: plugin for plugin in record["plugins"]}
    assert len(actual) == len(record["plugins"]), "duplicate plugin observations"
    for name, version in {"RiftOnlineProbe": "2.0", **expected}.items():
        assert name in actual, f"Paper did not load {name}"
        assert actual[name]["enabled"] is True, f"Paper disabled {name}"
        assert actual[name]["version"] == version, (name, actual[name]["version"], version)


def verify_signed(record, expected, marker, event):
    assert record["event"] == event
    assert record["uuid"] == expected["id"] and record["name"] == expected["name"]
    assert record["message"] == record["signed_message"] == marker, "signed content changed"
    assert record["signed"] is True and record["signature_bytes"] > 0, "missing player signature"
    assert record["signed_identity"] == expected["id"], "signature identity mismatch"
    if event == "chat":
        assert record.get("cancelled") is False, "missing uncancelled chat evidence"
    else:
        assert record.get("cancelled", False) is False, "plugin cancelled the message"


def verify_pack_status(rows, expected, pack_id, status):
    uuid.UUID(pack_id)
    relevant = [row for row in rows if row.get("event") == "resource_pack_status"
                and row.get("uuid") == expected["id"] and row.get("pack_id") == pack_id]
    assert relevant, "no matching player/pack response from Paper"
    states = [row["status"] for row in relevant]
    assert states[-1] == status, states
    if status == "SUCCESSFULLY_LOADED":
        assert "ACCEPTED" in states, states
        assert states.index("ACCEPTED") < len(states) - 1, states
        assert not set(states) & {"DECLINED", "FAILED_DOWNLOAD", "INVALID_URL", "FAILED_RELOAD", "DISCARDED"}, states
    return relevant
