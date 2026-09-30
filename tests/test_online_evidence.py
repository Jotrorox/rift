"""Fail closed when online acceptance evidence is absent, stale or unsigned."""

import copy
import hashlib
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import urllib.error
import urllib.request
import uuid
import zipfile

import manual_online as online
import online_evidence as evidence


class OnlineEvidenceTests(unittest.TestCase):
    profile = {"id": "07070707-0707-0707-0707-070707070707", "name": "Player"}

    def signed(self, event="chat"):
        return {"event": event, "uuid": self.profile["id"], "name": "Player",
                "message": "nonce", "signed_message": "nonce", "signed": True,
                "signature_bytes": 256, "signed_identity": self.profile["id"], "cancelled": False}

    def test_signed_evidence_rejects_unsigned_cancelled_or_other_identity(self):
        for event in ("chat", "signed_command"):
            evidence.verify_signed(self.signed(event), self.profile, "nonce", event)
            for field, value in (("signed", False), ("signature_bytes", 0), ("cancelled", True),
                                 ("uuid", str(uuid.uuid4())), ("signed_identity", str(uuid.uuid4())),
                                 ("name", "Impostor"), ("message", "stale"), ("signed_message", "rewritten"),
                                 ("event", "command")):
                with self.subTest(event=event, field=field), self.assertRaises(AssertionError):
                    evidence.verify_signed({**self.signed(event), field: value}, self.profile, "nonce", event)
        missing = self.signed()
        del missing["cancelled"]
        with self.assertRaises(AssertionError):
            evidence.verify_signed(missing, self.profile, "nonce", "chat")
        missing["event"] = "signed_command"
        evidence.verify_signed(missing, self.profile, "nonce", "signed_command")

    def test_plugin_inventory_requires_enabled_pinned_versions_and_probe(self):
        record = {"event": "plugins", "plugins": [
            {"name": "RiftOnlineProbe", "version": "2.0", "enabled": True},
            {"name": "LuckPerms", "version": "5.5.85", "enabled": True}]}
        expected = {"LuckPerms": "5.5.85"}
        evidence.verify_plugins(record, expected)
        for field, value in (("enabled", False), ("name", "Missing"), ("version", "newer")):
            bad = copy.deepcopy(record)
            bad["plugins"][1][field] = value
            with self.assertRaises(AssertionError):
                evidence.verify_plugins(bad, expected)
        for rows in (record["plugins"][1:], record["plugins"] * 2):
            with self.assertRaises(AssertionError):
                evidence.verify_plugins({"event": "plugins", "plugins": rows}, expected)

    def test_resource_pack_responses_are_correlated_and_failures_not_ignored(self):
        pack_id = str(uuid.uuid4())
        base = {"event": "resource_pack_status", "uuid": self.profile["id"], "pack_id": pack_id}
        rows = [{**base, "status": state} for state in ("ACCEPTED", "DOWNLOADED", "SUCCESSFULLY_LOADED")]
        self.assertEqual(evidence.verify_pack_status(rows, self.profile, pack_id, "SUCCESSFULLY_LOADED"), rows)
        for bad in (rows[1:], rows[::-1], [], [{**row, "uuid": str(uuid.uuid4())} for row in rows],
                    [{**base, "status": "FAILED_DOWNLOAD"}] + rows,
                    rows + [{**base, "status": "DISCARDED"}]):
            with self.assertRaises(AssertionError):
                evidence.verify_pack_status(bad, self.profile, pack_id, "SUCCESSFULLY_LOADED")
        evidence.verify_pack_status([{**base, "status": "DECLINED"}], self.profile, pack_id, "DECLINED")

    def test_pack_contents_hashes_and_loopback_server(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            (directory / "secret").write_text("must not be served")
            with evidence.resource_packs(directory) as packs:
                for role, metadata in packs.items():
                    with urllib.request.urlopen(metadata["url"], timeout=3) as response:
                        payload = response.read()
                    self.assertEqual(payload, evidence.pack_bytes(role))
                    self.assertEqual(payload, (directory / f"{role}.zip").read_bytes())
                    self.assertEqual(hashlib.sha1(payload).hexdigest(), metadata["sha1"])
                    self.assertEqual(hashlib.sha256(payload).hexdigest(), metadata["sha256"])
                    with zipfile.ZipFile(io.BytesIO(payload)) as archive:
                        self.assertEqual(archive.testzip(), None)
                        meta = json.loads(archive.read("pack.mcmeta"))
                        self.assertEqual(meta["pack"]["min_format"], [75, 0])
                        language = json.loads(archive.read("assets/minecraft/lang/en_us.json"))
                        self.assertEqual(language["item.minecraft.diamond"], f"RIFT {role.upper()} Diamond")
                base = packs["lobby"]["url"].rsplit("/", 1)[0]
                for path in ("/secret", "/../secret", "/", "/lobby.zip?file=secret"):
                    with self.assertRaises(urllib.error.HTTPError) as error:
                        urllib.request.urlopen(base + path, timeout=3)
                    self.assertEqual(error.exception.code, 404)
                    error.exception.close()
        self.assertNotEqual(evidence.pack_bytes("lobby"), evidence.pack_bytes("primary"))
        with self.assertRaises(ValueError):
            evidence.pack_bytes("other")

    def test_snapshot_ignores_interleaved_async_observations(self):
        desired = {"event": "snapshot", "name": "Player", "inventory": []}
        with patch.object(online, "records", side_effect=[[], [
                {"event": "snapshot", "name": "Other"}, desired, self.signed()]]), \
                patch.object(online, "console"):
            self.assertEqual(online.snapshot(object(), Path("unused"), "Player"), desired)

    def test_required_refusal_requires_offer_but_not_a_status_callback(self):
        pack_id = uuid.UUID("abababab-abab-abab-abab-abababababab")
        pack = {"url": "http://127.0.0.1:1234/lobby.zip", "sha1": "a" * 40}
        request = {"event": "resource_pack_request", "uuid": self.profile["id"],
                   "pack_id": str(pack_id), "required": True, **pack}
        result = {"manual_steps": {}, "passed": False}
        with patch.object(online.uuid, "uuid4", return_value=pack_id), \
                patch.object(online, "records", side_effect=[[], [request], [request]]), \
                patch.object(online, "console"), patch.object(online, "confirm"), \
                patch.object(online, "save_result"):
            online.pack_checkpoint(result, "lobby_required_declined", object(), Path("unused"),
                                   self.profile, pack, required=True, decline=True)
        observed = result["pack_checks"]["lobby_required_declined"]
        self.assertFalse(observed["status_callback_required"])
        self.assertEqual(observed["observations"], [])
        self.assertEqual(observed["request"], request)
        self.assertFalse(result["passed"])  # Caller still must observe connection closure.

        loaded = {"event": "resource_pack_status", "uuid": self.profile["id"],
                  "pack_id": str(pack_id), "status": "SUCCESSFULLY_LOADED"}
        with patch.object(online.uuid, "uuid4", return_value=pack_id), \
                patch.object(online, "records", side_effect=[[], [request], [request, loaded]]), \
                patch.object(online, "console"), patch.object(online, "confirm"), \
                self.assertRaises(AssertionError):
            online.pack_checkpoint(result, "lobby_required_declined", object(), Path("unused"),
                                   self.profile, pack, required=True, decline=True)

    def test_partial_jsonl_and_operator_confirmation_do_not_invent_success(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            plugin = directory / "plugins/RiftOnlineProbe"
            plugin.mkdir(parents=True)
            (plugin / "profiles.jsonl").write_text('{"event":"plugins"}\n{"event":')
            self.assertEqual(online.records(directory), [{"event": "plugins"}])
            result = {"logs": str(directory), "passed": False, "manual_steps": {}}
            for answer in ("", "pass", "FAIL"):
                with patch("builtins.input", return_value=answer), self.assertRaises(AssertionError):
                    online.confirm(result, "step", "fixture prompt")
            self.assertEqual(result["manual_steps"], {})
            with patch("builtins.input", return_value="PASS"):
                online.confirm(result, "step", "fixture prompt")
            saved = json.loads((directory / "result.json").read_text())
            self.assertFalse(saved["passed"])
            self.assertTrue(saved["manual_steps"]["step"]["passed"])
            self.assertFalse((directory / "result.tmp").exists())


if __name__ == "__main__":
    unittest.main()
