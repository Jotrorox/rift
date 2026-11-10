"""A schema-driven Minecraft client for every protocol from 1.12 to 26.3.

Packets are addressed by their minecraft-data names, encoded and decoded with
the pinned schema of the client's own protocol (tests/protodef.py). One reader
thread runs the whole session: login, configuration, play, and a return to
configuration when the server asks for it. It answers what a real client must
answer to stay connected (keep-alives, pings, teleports, chunk batches,
resource packs, window transactions, cookies, login plugin requests, known
packs, codes of conduct) and records every clientbound packet together with
its strict decode result. A packet the schema cannot decode, with trailing
bytes or with an unknown ID is a failure of the session; it is recorded, not
raised, so the scenario continues and all failures are reported together.
"""

import hashlib
import io
import json
import threading
import time
import uuid

from minecraft import Client, read_varint, teleport_acknowledgement
import protodef


def offline_uuid(name, pumpkin=False):
    if pumpkin:
        # Pumpkin's net::offline_uuid uses the first 16 SHA-256 bytes.
        return uuid.UUID(bytes=hashlib.sha256(name.encode()).digest()[:16])
    return uuid.UUID(bytes=hashlib.md5(f"OfflinePlayer:{name}".encode()).digest(), version=3)


def text_of(component):
    """Plain text of a chat component: JSON text, NBT compound, or a string."""
    if component is None:
        return ""
    if isinstance(component, str):
        stripped = component.strip()
        if stripped[:1] in ('{', '[', '"'):
            try:
                return text_of(json.loads(stripped))
            except ValueError:
                pass
        return component
    if isinstance(component, list):
        return "".join(text_of(part) for part in component)
    if isinstance(component, dict):
        if set(component) == {""}:  # A lone NBT string in a compound wrapper.
            return text_of(component[""])
        parts = [str(component.get("text", ""))]
        if "translate" in component:
            parts.append(str(component["translate"]))
            for argument in component.get("with", []) or []:
                parts.append(" " + text_of(argument))
        for extra in component.get("extra", []) or []:
            parts.append(text_of(extra))
        return "".join(parts)
    return str(component)


def jsonable(value, limit=512):
    """A JSON-safe copy of a decoded value with long byte strings shortened."""
    if isinstance(value, bytes):
        text = value.hex()
        return text if len(text) <= limit else f"{text[:limit]}...({len(value)} bytes)"
    if isinstance(value, dict):
        return {str(key): jsonable(entry, limit) for key, entry in value.items()}
    if isinstance(value, (list, tuple)):
        if len(value) > 4096:
            return [jsonable(entry, limit) for entry in value[:4096]] + [f"...({len(value)} entries)"]
        return [jsonable(entry, limit) for entry in value]
    if isinstance(value, float) and value != value:
        return "NaN"
    return value


def hexdump(data, limit=256):
    lines = []
    for offset in range(0, min(len(data), limit), 16):
        chunk = data[offset:offset + 16]
        lines.append(f"{offset:04x}  {chunk.hex(' '):<47}  "
                     + "".join(chr(b) if 32 <= b < 127 else "." for b in chunk))
    if len(data) > limit:
        lines.append(f"... {len(data) - limit} more bytes")
    return "\n".join(lines)


class Record:
    """One received packet."""
    __slots__ = ("index", "time", "state", "id", "body", "name", "value", "error", "step")

    def __init__(self, index, state, packet_id, body, step):
        self.index, self.time, self.state, self.id, self.body = index, time.monotonic(), state, packet_id, body
        self.name = self.value = self.error = None
        self.step = step

    def describe(self):
        return f"{self.state} {self.name or '?'} (id {self.id:#04x}, {len(self.body)} bytes)"


class SessionError(AssertionError):
    pass


class Session(Client):
    """One player connection."""

    STATES = ("login", "configuration", "play")

    def __init__(self, port, name, protocol, schema=None, label=None, pumpkin=False,
                 hostname="localhost", known_packs=()):
        self.lock = threading.RLock()  # The handshake in Client.__init__ already sends.
        self.changed = threading.Condition(self.lock)
        self.protocol = protocol
        self.schema = schema or protodef.schema_for(protocol)
        self.name = name
        self.label = label or name
        self.uuid = offline_uuid(name, pumpkin)
        self.known_packs = list(known_packs)
        self.decoders, self.encoders = {}, {}
        for state in self.STATES:
            if state in self.schema:
                self.decoders[state] = protodef.Decoder(self.schema, state, "clientbound")
                self.encoders[state] = protodef.Encoder(self.schema, state, "serverbound")
        self.records = []
        self.sent = []  # (time, state, name, value) for the packet log.
        self.state = "login"
        self.step = "login"
        self.failure = None  # Why the connection ended unexpectedly.
        self.closing = False
        self.entity_id = None
        self.position = None  # [x, y, z, yaw, pitch]
        self.dimension_changes = 0
        self.keepalives = 0
        self.teleports = 0
        self.chunks = set()  # Loaded (x, z) chunk columns.
        self.joined = threading.Event()
        self.loaded_sent = False
        self.ended = threading.Event()
        super().__init__(port, 2, protocol, hostname)
        self.deadline = time.monotonic() + 3600
        self.read_timeout = 90

    # Sending

    def send_packet(self, name, value=None, state=None):
        with self.lock:
            state = state or self.state
            packet_id, body = self.encoders[state].packet(name, value)
            self.sent.append((time.monotonic(), state, name, value))
            self.send(packet_id, body)

    def has(self, name, state="play", direction="serverbound"):
        codec = (self.encoders if direction == "serverbound" else self.decoders).get(state)
        return codec is not None and name in codec.ids

    def start(self):
        self.send_packet("login_start", {"username": self.name, "playerUUID": self.uuid.hex})
        threading.Thread(target=self.run, name=f"reader-{self.label}", daemon=True).start()

    # Receiving

    def run(self):
        try:
            while not self.closing:
                packet_id, body = self.receive()
                self.handle(packet_id, body)
        except Exception as error:  # Surface reader failures to the scenario.
            with self.lock:
                if not self.closing and self.failure is None:
                    self.failure = f"connection lost in {self.state}: {type(error).__name__}: {error}"
        finally:
            self.ended.set()
            with self.changed:
                self.changed.notify_all()

    def handle(self, packet_id, body):
        with self.lock:
            state = self.state
            record = Record(len(self.records), state, packet_id, body, self.step)
            decoder = self.decoders[state]
            record.name = decoder.names.get(packet_id)
            try:
                record.name, record.value = decoder.packet(packet_id, body)
            except protodef.DecodeError as error:
                record.error = str(error)
            except Exception as error:  # A decoder bug is still a failure to decode.
                record.error = f"decoder crashed: {type(error).__name__}: {error}"
            self.records.append(record)
            getattr(self, f"on_{state}")(record)
            self.changed.notify_all()

    def on_login(self, record):
        name, value = record.name, record.value or {}
        if name == "compress":
            self.threshold = value.get("threshold", read_varint(io.BytesIO(record.body)))
        elif name == "success":
            if self.protocol >= 764:
                self.send_packet("login_acknowledged")
                self.state = "configuration"
                self.step = "configuration"
                self.client_options()
            else:
                self.state = "play"
                self.step = "join"
        elif name == "login_plugin_request":
            self.send_packet("login_plugin_response", {"messageId": value["messageId"]})
        elif name == "cookie_request":
            self.send_packet("cookie_response", {"cookie": value.get("cookie")})
        elif name == "disconnect":
            self.failure = f"login disconnect: {self.reason(value.get('reason'))}"
        elif name == "encryption_begin":
            self.failure = "the server asked for encryption; it must run in offline mode"

    def on_configuration(self, record):
        name, value = record.name, record.value or {}
        if name == "select_known_packs":
            self.send_packet("select_known_packs", {"packs": self.known_packs})
        elif name == "keep_alive":
            self.send_packet("keep_alive", value or {"keepAliveId": 0})
        elif name == "ping":
            self.send_packet("pong", value)
        elif name in ("add_resource_pack", "resource_pack_send"):
            self.resource_pack(value)
        elif name == "cookie_request":
            self.send_packet("cookie_response", {"cookie": value.get("cookie")})
        elif name == "code_of_conduct":
            self.send_packet("accept_code_of_conduct")
        elif name == "disconnect":
            self.failure = f"configuration disconnect: {self.reason(value.get('reason'))}"
        elif name == "finish_configuration":
            self.send_packet("finish_configuration")
            self.state = "play"
            if self.step == "configuration":
                self.step = "join"

    def on_play(self, record):
        name, value = record.name, record.value
        if name is None:
            return
        if name == "keep_alive":
            # Echo raw so a keep-alive the schema cannot decode still answers.
            self.send(self.encoders["play"].ids["keep_alive"], record.body)
            self.keepalives += 1
        elif value is None:
            # Undecodable (already a failure): keep the session going if we can.
            if name == "position" and self.protocol != 777:
                self.send(self.encoders["play"].ids["teleport_confirm"],
                          teleport_acknowledgement(record.body, self.protocol))
                self.teleports += 1
                if self.entity_id is not None:
                    self.joined.set()
            elif name == "login" and len(record.body) >= 4:
                self.entity_id = int.from_bytes(record.body[:4], "big", signed=True)
                if self.protocol < 764:
                    self.client_options()
            return
        elif name == "login":
            self.entity_id = value.get("entityId")
            if self.protocol < 764:
                self.client_options()
        elif name == "position":
            self.teleport(value)
        elif name == "ping":
            self.send_packet("pong", value)
        elif name == "chunk_batch_finished":
            self.send_packet("chunk_batch_received", {"chunksPerTick": 10.0})
        elif name == "transaction" and not value.get("accepted"):
            self.send_packet("transaction", dict(value, accepted=True))
        elif name in ("resource_pack_send", "add_resource_pack"):
            self.resource_pack(value)
        elif name == "cookie_request":
            self.send_packet("cookie_response", {"cookie": value.get("cookie")})
        elif name == "map_chunk":
            self.chunks.add((value.get("x"), value.get("z")))
        elif name in ("unload_chunk", "forget_level_chunk"):
            self.chunks.discard((value.get("chunkX", value.get("x")), value.get("chunkZ", value.get("z"))))
        elif name == "respawn":
            self.dimension_changes += 1
            self.chunks.clear()
        elif name == "start_configuration":
            self.send_packet("configuration_acknowledged")
            self.state = "configuration"
        elif name == "kick_disconnect":
            self.failure = f"kicked: {self.reason(value.get('reason'))}"

    @staticmethod
    def reason(component):
        return text_of(component) or repr(component)

    def client_options(self):
        state = self.state
        settings = {"locale": "en_us", "viewDistance": 2, "chatFlags": 0, "chatColors": True,
                    "skinParts": 127, "mainHand": 1, "enableTextFiltering": False,
                    "disableTextFiltering": False, "enableServerListing": True, "particles": 0}
        self.send_packet("settings", settings, state)
        brand = "minecraft:brand" if self.protocol >= 393 else "MC|Brand"
        self.send_packet("custom_payload", {"channel": brand, "data": b"\x09rift-test"}, state)

    def resource_pack(self, value):
        for result in (3, 0):  # Accepted, then successfully loaded.
            self.send_packet("resource_pack_receive", {"uuid": value.get("uuid"), "result": result})

    def teleport(self, value):
        flags = value.get("flags", 0)
        if isinstance(flags, dict):
            relative = [flags.get(axis, False) for axis in ("x", "y", "z", "yaw", "pitch")]
        else:
            relative = [bool(flags & (1 << bit)) for bit in range(5)]
        target = [value["x"], value["y"], value["z"], value["yaw"], value["pitch"]]
        if self.position is not None:
            target = [base + offset if rel else offset
                      for base, offset, rel in zip(self.position, target, relative)]
        self.position = target
        x, y, z, yaw, pitch = target
        self.send_packet("teleport_confirm", {"teleportId": value["teleportId"],
                                              "x": x, "y": y, "z": z, "yaw": yaw, "pitch": pitch})
        # Like a real client: confirm, then report the position. From 26.3 the
        # confirmation carries the position itself and counts as this tick's
        # movement (one per client tick, ServerGamePacketListenerImpl
        # handleMovePlayer), so the report follows in the next tick.
        if self.protocol >= 777:
            self.tick_end()
        self.send_packet("position_look", {"x": x, "y": y, "z": z, "yaw": yaw, "pitch": pitch,
                                           "onGround": True, "flags": {"onGround": True}})
        self.tick_end()
        if self.has("player_loaded") and not self.loaded_sent:
            self.send_packet("player_loaded")
            self.loaded_sent = True
        self.teleports += 1
        if self.entity_id is not None:
            self.joined.set()

    # Scenario helpers

    def check(self):
        """Raise if the connection failed."""
        with self.lock:
            if self.failure:
                raise SessionError(f"{self.label}: {self.failure}")

    def wait(self, predicate, what, timeout=15, since=0):
        """The first record from index `since` satisfying predicate(record)."""
        deadline = time.monotonic() + timeout
        with self.changed:
            index = since
            while True:
                while index < len(self.records):
                    record = self.records[index]
                    index += 1
                    if predicate(record):
                        return record
                self.check()
                if self.ended.is_set():
                    raise SessionError(f"{self.label}: connection closed while waiting for {what}")
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise SessionError(f"{self.label}: timed out after {timeout}s waiting for {what}")
                self.changed.wait(min(remaining, 0.5))

    def wait_packet(self, names, what=None, timeout=15, since=0, where=None):
        names = {names} if isinstance(names, str) else set(names)
        return self.wait(lambda record: record.name in names and record.value is not None
                         and (where is None or where(record.value)),
                         what or "/".join(sorted(names)), timeout, since)

    def mark(self):
        with self.lock:
            return len(self.records)

    def packets(self, names=None, since=0):
        with self.lock:
            records = self.records[since:]
        if names is None:
            return records
        names = {names} if isinstance(names, str) else set(names)
        return [record for record in records if record.name in names and record.value is not None]

    def join(self, timeout=60):
        self.start()
        deadline = time.monotonic() + timeout
        while not self.joined.wait(0.2):
            self.check()
            if self.ended.is_set():
                raise SessionError(f"{self.label}: connection closed before joining")
            if time.monotonic() > deadline:
                raise SessionError(f"{self.label}: did not join within {timeout}s "
                                   f"(state {self.state}, {len(self.records)} packets)")
        self.step = "join"

    def tick_end(self):
        """End a client tick (1.21.2+). 26.3 allows one position per client
        tick and resets that on this packet (handleClientTickEnd)."""
        if self.has("tick_end"):
            self.send_packet("tick_end")

    def move(self, dx=0.0, dy=0.0, dz=0.0):
        with self.lock:
            x, y, z, yaw, pitch = self.position
            self.position = [x + dx, y + dy, z + dz, yaw, pitch]
            self.send_packet("position", {"x": x + dx, "y": y + dy, "z": z + dz,
                                          "onGround": True, "flags": {"onGround": True}})
            self.tick_end()

    def close(self):
        with self.lock:
            self.closing = True
        try:
            self.socket.shutdown(2)
        except OSError:
            pass
        self.ended.wait(5)
        self.reader.close()
        self.socket.close()

    def failures(self):
        """Packets that failed to decode."""
        with self.lock:
            return [record for record in self.records if record.error]

    def write_log(self, path):
        with self.lock:
            records, sent = list(self.records), list(self.sent)
        start = records[0].time if records else time.monotonic()
        with open(path, "w") as out:
            for record in records:
                entry = {"i": record.index, "t": round(record.time - start, 3), "state": record.state,
                         "id": record.id, "name": record.name, "step": record.step,
                         "size": len(record.body)}
                if record.error:
                    entry["error"] = record.error
                    entry["hex"] = record.body.hex()  # Whole body: --redecode replays it.
                else:
                    entry["value"] = jsonable(record.value)
                out.write(json.dumps(entry) + "\n")
            for when, state, name, value in sent:
                out.write(json.dumps({"t": round(when - start, 3), "sent": name, "state": state,
                                      "value": jsonable(value)}) + "\n")
