"""minecraft-data style schemas for 26.2 (776) and 26.3 (777), derived from
the corrected 26.1 schema. minecraft-data covers neither yet.

Derivation, one release at a time (26.3 builds on 26.2):
  1. Packet IDs: the official server's data generator report packets.json
     gives every state's packets in ID order. Mojang names map to
     minecraft-data names through 26.1.2's report and schema (same index,
     same packet); a packet new in the release gets a definition from
     PATCHES or is marked unverified.
  2. Registry-backed mappers (data component types, particle types, command
     argument types, ...) are rebuilt from the release's registries.json by
     registry name. New entries keep the switch they select complete: a case
     from PATCHES, or an unverified case.
  3. PATCHES: layouts that changed, each checked with `javap -c -private`
     against the release's unobfuscated server classes (the STREAM_CODEC
     composition or write method named in `why`).
  4. UNVERIFIED: types and packets whose stream codec classes changed in a way
     not modelled here. Decoding one raises protodef.Unverified, which the
     matrix reports as UNVERIFIED (never as a pass, never as a failure).

How the change list was found: every class referencing StreamCodec,
FriendlyByteBuf or ByteBufCodecs was disassembled in both releases and the
codec-relevant bytecode (static initialisers, methods taking a buffer)
compared; classes with no codec difference keep 26.1's layout. Many 26.3
classes only moved from write methods to StreamCodec.composite with the same
order; those were compared field by field and need no patch.
"""

import copy
import json

import protodef

BASE = "26.1.2"
DERIVED = {776: "26.2", 777: "26.3"}
ORDER = ["26.2", "26.3"]


def _container(*fields):
    return ["container", [{"name": name, "type": kind} for name, kind in fields]]


def unverified(reason):
    return ["unverified", reason]


# Mojang packet names (packets.json) of packets new in a release -> the
# minecraft-data style name used here.
NEW_PACKETS = {
    "minecraft:spectator_action": "spectator_action",
    "minecraft:post_effects": "post_effects",
    "minecraft:add_transient_block": "add_transient_block",
    "minecraft:swing_animation": "swing_animation",
    "minecraft:punch": "punch",
}

# Registry-backed mappers: (path, registry, path of the switch it drives or None).
SLOT_COMPONENTS = ["types", "SlotComponent", 1, 1, "type", 1, "fields"]
PARTICLE_DATA = ["types", "Particle", 1, 1, "type", 1, "fields"]
PARSER_PROPERTIES = ["types", "command_node", 1, 3, "type", 1, "fields", "2", 1, 2, "type", 1, "fields"]
MAPPERS = [
    (["types", "SlotComponentType", 1], "minecraft:data_component_type", SLOT_COMPONENTS),
    (["types", "Particle", 1, 0, "type", 1], "minecraft:particle_type", PARTICLE_DATA),
    (["types", "command_node", 1, 3, "type", 1, "fields", "2", 1, 1, "type", 1],
     "minecraft:command_argument_type", PARSER_PROPERTIES),
    (["types", "ItemConsumeEffect", 1, 0, "type", 1], "minecraft:consume_effect_type",
     ["types", "ItemConsumeEffect", 1, 1, "type", 1, "fields"]),
    (["play", "toClient", "types", "SlotDisplay", 1, 0, "type", 1], "minecraft:slot_display", None),
    (["play", "toClient", "types", "RecipeDisplay", 1, 0, "type", 1], "minecraft:recipe_display", None),
]

ITEM_STACK_TEMPLATE = "ItemStackTemplate"
VEC3 = _container(("x", "f64"), ("y", "f64"), ("z", "f64"))
# 26.3 VecDelta (net.minecraft.network.protocol.game.VecDelta): three shorts
# when the packed step count is 0, else `count` (VarInt, short, short, short).
STEP = _container(("ticks", "varint"), ("dX", "i16"), ("dY", "i16"), ("dZ", "i16"))


def move_fields(rotation):
    fields = [{"name": "entityId", "type": "varint"},
              {"name": "movement", "type": "MovementDelta"}]
    if rotation:
        fields += [{"name": "yaw", "type": "i8"}, {"name": "pitch", "type": "i8"}]
    return ["container", fields]


def _field(container, name):
    for field in container[1]:
        if field.get("name") == name:
            return field
    raise KeyError(f"patch target field {name} is missing")


def _insert_before(container, before, field):
    index = container[1].index(_field(container, before))
    container[1].insert(index, field)


# Each patch: (description of what was checked, function(schema)).
def _patch_26_2(schema):
    play, login = schema["play"]["toClient"]["types"], schema["login"]["toClient"]["types"]
    # ClientboundLoginPacket.write: ... commonPlayerSpawnInfo, onlineMode, enforcesSecureChat.
    _insert_before(play["packet_login"], "enforcesSecureChat", {"name": "onlineMode", "type": "bool"})
    # ClientboundLoginFinishedPacket: GAME_PROFILE then UUIDUtil.STREAM_CODEC (session ID).
    login["packet_success"][1].append({"name": "sessionId", "type": "UUID"})
    # ClientboundSetPlayerTeamPacket$Parameters.STREAM_CODEC: display name, prefix,
    # suffix, name tag visibility, collision rule, optional TeamColor (idMapper),
    # options byte.
    teams = play["packet_teams"][1][2]["type"][1]["fields"]
    for mode in ("add", "change"):
        old = {field["name"]: field["type"] for field in teams[mode][1]}
        teams[mode] = _container(
            ("name", old["name"]), ("prefix", old["prefix"]), ("suffix", old["suffix"]),
            ("nameTagVisibility", old["nameTagVisibility"]), ("collisionRule", old["collisionRule"]),
            ("color", ["option", "varint"]), ("flags", old["flags"]))
    # ServerboundSpectatorActionPacket: ByteBufCodecs.OPTIONAL_VAR_INT.
    schema["play"]["toServer"]["types"]["packet_spectator_action"] = _container(("spectateEntityId", "optvarint"))
    # SulfurCubeContent: ItemStackTemplate.STREAM_CODEC mapped.
    _at(schema, SLOT_COMPONENTS)["sulfur_cube_content"] = ITEM_STACK_TEMPLATE
    # GeyserParticleOptions: INT; GeyserBaseParticleOptions: INT, FLOAT
    # (ParticleTypes registers geyser/geyser_plume and geyser_base/geyser_poof with them).
    particles = _at(schema, PARTICLE_DATA)
    for name in ("geyser", "geyser_plume"):
        particles[name] = _container(("waterBlocks", "i32"))
    for name in ("geyser_base", "geyser_poof"):
        particles[name] = _container(("waterBlocks", "i32"), ("burstImpulseBase", "f32"))
    # minecraft:team_color (replaces minecraft:color) has no properties.
    _at(schema, PARSER_PROPERTIES)["minecraft:team_color"] = "void"


def _patch_26_3(schema):
    types = schema["types"]
    play = schema["play"]["toClient"]["types"]
    server = schema["play"]["toServer"]["types"]
    # CommonPlayerSpawnInfo.STREAM_CODEC: GameType.STREAM_CODEC (idMapper VarInt)
    # and GameType.OPTIONAL_STREAM_CODEC (OPTIONAL_VAR_INT) replace two bytes.
    spawn = _spawn_info(schema)
    _field(spawn, "gamemode")["type"] = "varint"
    _field(spawn, "previousGamemode")["type"] = "optvarint"
    # ServerboundAcceptTeleportationPacket: VAR_INT, DOUBLE x3, FLOAT x2.
    server["packet_teleport_confirm"] = _container(
        ("teleportId", "varint"), ("x", "f64"), ("y", "f64"), ("z", "f64"), ("yaw", "f32"), ("pitch", "f32"))
    # ClientboundMoveEntityPacket$Pos/$PosRot: entity ID, VarInt packProperties
    # (bit 0 on ground, step count above it), VecDelta; $Rot: on ground before
    # the rotation bytes.
    types["MovementDelta"] = "native"
    play["packet_rel_entity_move"] = move_fields(False)
    play["packet_entity_move_look"] = move_fields(True)
    play["packet_entity_look"] = _container(("entityId", "varint"), ("onGround", "bool"),
                                            ("yaw", "i8"), ("pitch", "i8"))
    # ClientboundEntityPositionSyncPacket: VAR_INT, PositionPath (dispatch on an
    # idMapper type: LINEAR Vec3, STEPPED list of (Vec3, VAR_INT)), yaw, pitch, on ground.
    types["PositionPath"] = _container(
        ("type", ["mapper", {"type": "varint", "mappings": {"0": "linear", "1": "stepped"}}]),
        ("path", ["switch", {"compareTo": "type", "fields": {
            "linear": VEC3,
            "stepped": ["array", {"countType": "varint", "type": _container(("position", VEC3), ("ticks", "varint"))}]}}]))
    play["packet_sync_entity_position"] = _container(
        ("entityId", "varint"), ("path", "PositionPath"), ("yaw", "f32"), ("pitch", "f32"), ("onGround", "bool"))
    # ClientboundLevelParticlesPacket.STREAM_CODEC: particle first, BOOL x2,
    # DOUBLE x3, FLOAT x6 (distances, then per-axis max speeds), VAR_INT count,
    # RandomizationType (idMapper: default, alternative, alternative_with_speed).
    old = {field["name"]: field["type"] for field in play["packet_world_particles"][1]}
    play["packet_world_particles"] = _container(
        ("particle", old["particle"]), ("longDistance", "bool"), ("alwaysShow", "bool"),
        ("x", "f64"), ("y", "f64"), ("z", "f64"), ("offsetX", "f32"), ("offsetY", "f32"), ("offsetZ", "f32"),
        ("velocityX", "f32"), ("velocityY", "f32"), ("velocityZ", "f32"), ("amount", "varint"),
        ("randomization", ["mapper", {"type": "varint", "mappings": {
            "0": "default", "1": "alternative", "2": "alternative_with_speed"}}]))
    # ByteBufCodecs.BIT_SET ($15) is now BitSet.toByteArray with a VarInt length
    # (was a VarInt-counted long array): every light mask of chunk and light packets.
    masks = ("skyLightMask", "blockLightMask", "emptySkyLightMask", "emptyBlockLightMask")
    for name in ("packet_map_chunk", "packet_update_light"):
        for mask in masks:
            _field(play[name], mask)["type"] = ["buffer", {"countType": "varint"}]
    # SignTextSlot.STREAM_CODEC is an idMapper VarInt (was a boolean);
    # ServerboundSignUpdatePacket: position, four lines, then the slot.
    play["packet_open_sign_entity"] = _container(("location", "position"), ("slot", "varint"))
    server["packet_update_sign"] = _container(
        ("location", "position"), ("text1", "string"), ("text2", "string"), ("text3", "string"),
        ("text4", "string"), ("slot", "varint"))
    # EntityDataSerializers registers DYE_COLOR (DyeColor.STREAM_CODEC) last.
    entry = types["entityMetadataEntry"][1]
    mappings = entry[1]["type"][1]["mappings"]
    mappings[str(len(mappings))] = "dye_color"
    entry[2]["type"][1]["fields"]["dye_color"] = "DyeColor"
    # ClientboundExplodePacket.STREAM_CODEC appends ByteBufCodecs.BOOL (playSound).
    play["packet_explosion"][1].append({"name": "playSound", "type": "bool"})
    # The new argument types are all SingletonArgumentInfo (ArgumentTypeInfos):
    # no properties on the wire.
    for name in ("minecraft:context_float_provider", "minecraft:context_int_provider",
                 "minecraft:slot_source", "minecraft:feature", "minecraft:swing_animation"):
        _at(schema, PARSER_PROPERTIES)[name] = "void"
    # New packets.
    for state in ("configuration", "play"):
        # ClientboundPostEffectsPacket: list of Identifier.
        schema[state]["toClient"]["types"]["packet_post_effects"] = _container(
            ("effects", ["array", {"countType": "varint", "type": "string"}]))
    # ClientboundAddTransientBlockPacket: block position, block state (idMapper VarInt).
    play["packet_add_transient_block"] = _container(("location", "position"), ("type", "varint"))
    # ClientboundSwingAnimationPacket: VAR_INT, InteractionHand (idMapper), and
    # SwingAnimation.STREAM_CODEC: SwingAnimationType (idMapper) and a VAR_INT duration.
    play["packet_swing_animation"] = _container(
        ("entityId", "varint"), ("hand", "varint"),
        ("animation", ["mapper", {"type": "varint", "mappings": {"0": "none", "1": "whack", "2": "stab"}}]),
        ("duration", "varint"))
    # ServerboundPunchPacket: StreamCodec.unit (no body).
    server["packet_punch"] = ["container", []]
    # ClientboundUpdateAdvancementsPacket: the added list holds
    # PositionedAdvancement (AdvancementHolder, FLOAT x, FLOAT y); DisplayInfo
    # no longer writes the position after the background. AdvancementType is
    # an idMapper VarInt as before; CriterionProgress is optional INSTANT (a
    # long), the same bytes as writeNullable/writeInstant.
    added = _field(play["packet_advancements"], "advancementMapping")["type"][1]
    display = _field(_field(added["type"], "value")["type"], "displayData")["type"][1]
    display[1] = [field for field in display[1] if field["name"] not in ("xCord", "yCord")]
    added["type"][1] += [{"name": "x", "type": "f32"}, {"name": "y", "type": "f32"}]
    # SlotDisplay$TagSlotDisplay: ByteBufCodecs.holderSet(ITEM) (was
    # TagKey.streamCodec): a tag name or a list of item IDs, like IDSet.
    play["SlotDisplay"][1][1]["type"][1]["fields"]["tag"] = "IDSet"


def _spawn_info(schema):
    for section in (schema["types"], schema["play"]["toClient"]["types"]):
        if "SpawnInfo" in section:
            return section["SpawnInfo"]
    login = schema["play"]["toClient"]["types"]["packet_login"]
    kind = _field(login, "worldState")["type"]
    raise KeyError(f"SpawnInfo type {kind} not found")


PATCHES = {"26.2": _patch_26_2, "26.3": _patch_26_3}

# Codec classes that changed and are not modelled: (schema path, reason).
UNVERIFIED = {
    "26.2": [],
    "26.3": [
        (["configuration", "toClient", "types", "packet_show_dialog"], "Dialog changed"),
        (["play", "toClient", "types", "packet_show_dialog"], "Dialog changed"),
        (SLOT_COMPONENTS + ["pot_decorations"], "PotDecorations changed (DecoratedPotPatterns)"),
        (SLOT_COMPONENTS + ["instrument"], "Instrument changed"),
        (SLOT_COMPONENTS + ["trim"], "TrimMaterial changed"),
        (SLOT_COMPONENTS + ["provides_trim_material"], "TrimMaterial changed"),
        (["play", "toServer", "types", "packet_chat_session_update"], "not compared in detail"),
        (["play", "toServer", "types", "packet_vehicle_move"], "PositionAndRotation not compared in detail"),
        (["play", "toServer", "types", "packet_update_structure_block"], "ServerboundSetStructureBlockPacket changed"),
    ],
}


def _at(document, path):
    for key in path:
        document = document[key]
    return document


def _ordered(entries):
    return [name for name, _ in sorted(entries.items(), key=lambda entry: entry[1]["protocol_id"])]


def _reports(version):
    import matrix_registries
    from matrix_fixtures import resolve
    path = matrix_registries.reports(resolve(f"vanilla-{version}"))[2]
    return (json.loads((path / "registries.json").read_text()),
            json.loads((path / "packets.json").read_text()))


def _repacket(schema, old_packets, new_packets):
    """Renumber every configuration and play packet from packets.json."""
    for state in ("configuration", "play"):
        for direction, side in (("clientbound", "toClient"), ("serverbound", "toServer")):
            section = schema[state][side]["types"]
            packet = section["packet"][1]
            mapper = packet[0]["type"][1]["mappings"]
            switch = packet[1]["type"][1]["fields"]
            old = _ordered(old_packets[state][direction])
            by_index = {int(key, 0): value for key, value in mapper.items()}
            if len(by_index) != len(old):
                raise ValueError(f"{state} {direction}: schema has {len(by_index)} packets, report {len(old)}")
            names = {mojang: by_index[index] for index, mojang in enumerate(old)}
            new_mapper, new_switch = {}, {}
            for index, mojang in enumerate(_ordered(new_packets[state][direction])):
                name = names.get(mojang) or NEW_PACKETS.get(mojang)
                if name is None:
                    name = mojang.removeprefix("minecraft:")
                new_mapper[f"0x{index:02x}"] = name
                # Keep the schema's own type names (not always packet_<name>).
                new_switch[name] = switch.get(name, f"packet_{name}")
                if new_switch[name] not in section and new_switch[name] not in schema["types"]:
                    section[new_switch[name]] = unverified(f"{mojang} is new and not modelled")
            packet[0]["type"][1]["mappings"] = new_mapper
            packet[1]["type"][1]["fields"] = new_switch


def _remap(schema, old_registries, new_registries):
    for path, registry, switch_path in MAPPERS:
        mapper = _at(schema, path)
        old_names = [mapper["mappings"][str(index)] for index in range(len(mapper["mappings"]))]
        old_registry = _ordered(old_registries[registry]["entries"])
        if len(old_registry) != len(old_names):
            raise ValueError(f"{registry}: schema mapper and report differ in size")
        by_registry = dict(zip(old_registry, old_names))
        prefixed = old_names[0].startswith("minecraft:") or ":" in old_names[0] and registry.endswith("argument_type")
        new_names = []
        for name in _ordered(new_registries[registry]["entries"]):
            if name in by_registry:
                new_names.append(by_registry[name])
            else:
                new_names.append(name if prefixed else name.removeprefix("minecraft:"))
        mapper["mappings"] = {str(index): name for index, name in enumerate(new_names)}
        if switch_path:
            cases = _at(schema, switch_path)
            for name in set(new_names) - set(old_names):
                cases.setdefault(name, unverified(f"{registry} entry {name} is new"))


def derive(version):
    schema = copy.deepcopy(protodef.schema(BASE))
    old_registries, old_packets = _reports(BASE)
    for step in ORDER[:ORDER.index(version) + 1]:
        registries, packets = _reports(step)
        _repacket(schema, old_packets, packets)
        _remap(schema, old_registries, registries)
        PATCHES[step](schema)  # After remapping: patches fill new switch cases.
        for path, reason in UNVERIFIED[step]:
            parent = _at(schema, path[:-1])
            if path[-1] not in parent:
                raise KeyError(f"unverified target {path} is missing")
            parent[path[-1]] = unverified(f"{step}: {reason}")
        old_registries, old_packets = registries, packets
    return schema


def movement_delta(reader):
    """26.3 packed movement: VarInt (bit 0 on ground, step count above), then
    a VecDelta."""
    properties = reader.varint()
    steps = (properties & 0xFFFFFFFF) >> 1
    value = {"onGround": bool(properties & 1)}
    if steps == 0:
        value.update(dX=_short(reader), dY=_short(reader), dZ=_short(reader))
    else:
        if steps > reader.remaining() // 7:
            raise protodef.DecodeError(f"{steps} movement steps in {reader.remaining()} bytes")
        value["steps"] = [{"ticks": reader.varint(), "dX": _short(reader), "dY": _short(reader),
                           "dZ": _short(reader)} for _ in range(steps)]
    return value


def _short(reader):
    return int.from_bytes(reader.take(2), "big", signed=True)


protodef.NATIVES["MovementDelta"] = movement_delta
