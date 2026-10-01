#!/usr/bin/env python3
"""Real vanilla, Paper and Pumpkin integration tests; Python 3.11+ and the fixture-specific Java runtime.

Downloads pinned official jars, verifies checksums, and runs isolated loopback
servers. Packet layouts: https://github.com/PrismarineJS/minecraft-data/tree/master/data/pc/1.21.11
"""

import argparse
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
import hashlib
import io
import json
import os
from pathlib import Path
import platform
import socket
import struct
import subprocess
import time
import tempfile
import traceback
import urllib.request
import uuid
import zlib

ROOT = Path(__file__).resolve().parents[1]
CACHE = ROOT / "target" / "minecraft"
SERVERS = json.loads((ROOT / "tests" / "servers.json").read_text())
# Only packet IDs used by this client. Keep these explicit: an upstream protocol
# change must fail visibly, not silently reduce the test to a status check.
# 26.2/26.3 IDs are checked against Mojang's pinned server artifacts.
PROTOCOLS = {
    47: dict(  # 1.8.9
        known_packs=-1, join=0x1, respawn=0x7, position=0x8,
        chunk=0x21, chunk_bulk=0x26, keepalive=0x0, disconnect=0x40,
        start_configuration=-1, system_chat=0x2, border=0x44, payload=0x3f,
        commands=-1, batch_finished=-1, keepalive_reply=0x0, loaded=None,
        batch_reply=-1, configuration_ack=-1, command=0x1, signed_command=-1,
        chat=0x1, chat_session=-1, settings=0x15, server_payload=0x17),

    107: dict(  # 1.9
        known_packs=-1, join=0x23, respawn=0x33, position=0x2e,
        chunk=0x20, chunk_bulk=-1, keepalive=0x1f, disconnect=0x1a,
        start_configuration=-1, system_chat=0xf, border=0x35, payload=0x18,
        commands=-1, batch_finished=-1, keepalive_reply=0xb, loaded=None,
        batch_reply=-1, configuration_ack=-1, command=0x2, signed_command=-1,
        chat=0x2, chat_session=-1, settings=0x4, server_payload=0x9),

    108: dict(  # 1.9.1
        known_packs=-1, join=0x23, respawn=0x33, position=0x2e,
        chunk=0x20, chunk_bulk=-1, keepalive=0x1f, disconnect=0x1a,
        start_configuration=-1, system_chat=0xf, border=0x35, payload=0x18,
        commands=-1, batch_finished=-1, keepalive_reply=0xb, loaded=None,
        batch_reply=-1, configuration_ack=-1, command=0x2, signed_command=-1,
        chat=0x2, chat_session=-1, settings=0x4, server_payload=0x9),

    109: dict(  # 1.9.2
        known_packs=-1, join=0x23, respawn=0x33, position=0x2e,
        chunk=0x20, chunk_bulk=-1, keepalive=0x1f, disconnect=0x1a,
        start_configuration=-1, system_chat=0xf, border=0x35, payload=0x18,
        commands=-1, batch_finished=-1, keepalive_reply=0xb, loaded=None,
        batch_reply=-1, configuration_ack=-1, command=0x2, signed_command=-1,
        chat=0x2, chat_session=-1, settings=0x4, server_payload=0x9),

    110: dict(  # 1.9.4
        known_packs=-1, join=0x23, respawn=0x33, position=0x2e,
        chunk=0x20, chunk_bulk=-1, keepalive=0x1f, disconnect=0x1a,
        start_configuration=-1, system_chat=0xf, border=0x35, payload=0x18,
        commands=-1, batch_finished=-1, keepalive_reply=0xb, loaded=None,
        batch_reply=-1, configuration_ack=-1, command=0x2, signed_command=-1,
        chat=0x2, chat_session=-1, settings=0x4, server_payload=0x9),

    210: dict(  # 1.10.2
        known_packs=-1, join=0x23, respawn=0x33, position=0x2e,
        chunk=0x20, chunk_bulk=-1, keepalive=0x1f, disconnect=0x1a,
        start_configuration=-1, system_chat=0xf, border=0x35, payload=0x18,
        commands=-1, batch_finished=-1, keepalive_reply=0xb, loaded=None,
        batch_reply=-1, configuration_ack=-1, command=0x2, signed_command=-1,
        chat=0x2, chat_session=-1, settings=0x4, server_payload=0x9),

    315: dict(  # 1.11
        known_packs=-1, join=0x23, respawn=0x33, position=0x2e,
        chunk=0x20, chunk_bulk=-1, keepalive=0x1f, disconnect=0x1a,
        start_configuration=-1, system_chat=0xf, border=0x35, payload=0x18,
        commands=-1, batch_finished=-1, keepalive_reply=0xb, loaded=None,
        batch_reply=-1, configuration_ack=-1, command=0x2, signed_command=-1,
        chat=0x2, chat_session=-1, settings=0x4, server_payload=0x9),

    316: dict(  # 1.11.2
        known_packs=-1, join=0x23, respawn=0x33, position=0x2e,
        chunk=0x20, chunk_bulk=-1, keepalive=0x1f, disconnect=0x1a,
        start_configuration=-1, system_chat=0xf, border=0x35, payload=0x18,
        commands=-1, batch_finished=-1, keepalive_reply=0xb, loaded=None,
        batch_reply=-1, configuration_ack=-1, command=0x2, signed_command=-1,
        chat=0x2, chat_session=-1, settings=0x4, server_payload=0x9),

    335: dict(  # 1.12
        known_packs=-1, join=0x23, respawn=0x34, position=0x2e,
        chunk=0x20, chunk_bulk=-1, keepalive=0x1f, disconnect=0x1a,
        start_configuration=-1, system_chat=0xf, border=0x37, payload=0x18,
        commands=-1, batch_finished=-1, keepalive_reply=0xc, loaded=None,
        batch_reply=-1, configuration_ack=-1, command=0x3, signed_command=-1,
        chat=0x3, chat_session=-1, settings=0x5, server_payload=0xa),

    338: dict(  # 1.12.1
        known_packs=-1, join=0x23, respawn=0x35, position=0x2f,
        chunk=0x20, chunk_bulk=-1, keepalive=0x1f, disconnect=0x1a,
        start_configuration=-1, system_chat=0xf, border=0x38, payload=0x18,
        commands=-1, batch_finished=-1, keepalive_reply=0xb, loaded=None,
        batch_reply=-1, configuration_ack=-1, command=0x2, signed_command=-1,
        chat=0x2, chat_session=-1, settings=0x4, server_payload=0x9),

    340: dict(  # 1.12.2
        known_packs=-1, join=0x23, respawn=0x35, position=0x2f,
        chunk=0x20, chunk_bulk=-1, keepalive=0x1f, disconnect=0x1a,
        start_configuration=-1, system_chat=0xf, border=0x38, payload=0x18,
        commands=-1, batch_finished=-1, keepalive_reply=0xb, loaded=None,
        batch_reply=-1, configuration_ack=-1, command=0x2, signed_command=-1,
        chat=0x2, chat_session=-1, settings=0x4, server_payload=0x9),

    393: dict(  # 1.13
        known_packs=-1, join=0x25, respawn=0x38, position=0x32,
        chunk=0x22, chunk_bulk=-1, keepalive=0x21, disconnect=0x1b,
        start_configuration=-1, system_chat=0xe, border=0x3b, payload=0x19,
        commands=0x11, batch_finished=-1, keepalive_reply=0xe, loaded=None,
        batch_reply=-1, configuration_ack=-1, command=0x2, signed_command=-1,
        chat=0x2, chat_session=-1, settings=0x4, server_payload=0xa),

    401: dict(  # 1.13.1
        known_packs=-1, join=0x25, respawn=0x38, position=0x32,
        chunk=0x22, chunk_bulk=-1, keepalive=0x21, disconnect=0x1b,
        start_configuration=-1, system_chat=0xe, border=0x3b, payload=0x19,
        commands=0x11, batch_finished=-1, keepalive_reply=0xe, loaded=None,
        batch_reply=-1, configuration_ack=-1, command=0x2, signed_command=-1,
        chat=0x2, chat_session=-1, settings=0x4, server_payload=0xa),

    404: dict(  # 1.13.2
        known_packs=-1, join=0x25, respawn=0x38, position=0x32,
        chunk=0x22, chunk_bulk=-1, keepalive=0x21, disconnect=0x1b,
        start_configuration=-1, system_chat=0xe, border=0x3b, payload=0x19,
        commands=0x11, batch_finished=-1, keepalive_reply=0xe, loaded=None,
        batch_reply=-1, configuration_ack=-1, command=0x2, signed_command=-1,
        chat=0x2, chat_session=-1, settings=0x4, server_payload=0xa),

    477: dict(  # 1.14
        known_packs=-1, join=0x25, respawn=0x3a, position=0x35,
        chunk=0x21, chunk_bulk=-1, keepalive=0x20, disconnect=0x1a,
        start_configuration=-1, system_chat=0xe, border=0x3d, payload=0x18,
        commands=0x11, batch_finished=-1, keepalive_reply=0xf, loaded=None,
        batch_reply=-1, configuration_ack=-1, command=0x3, signed_command=-1,
        chat=0x3, chat_session=-1, settings=0x5, server_payload=0xb),

    480: dict(  # 1.14.1
        known_packs=-1, join=0x25, respawn=0x3a, position=0x35,
        chunk=0x21, chunk_bulk=-1, keepalive=0x20, disconnect=0x1a,
        start_configuration=-1, system_chat=0xe, border=0x3d, payload=0x18,
        commands=0x11, batch_finished=-1, keepalive_reply=0xf, loaded=None,
        batch_reply=-1, configuration_ack=-1, command=0x3, signed_command=-1,
        chat=0x3, chat_session=-1, settings=0x5, server_payload=0xb),

    485: dict(  # 1.14.2
        known_packs=-1, join=0x25, respawn=0x3a, position=0x35,
        chunk=0x21, chunk_bulk=-1, keepalive=0x20, disconnect=0x1a,
        start_configuration=-1, system_chat=0xe, border=0x3d, payload=0x18,
        commands=0x11, batch_finished=-1, keepalive_reply=0xf, loaded=None,
        batch_reply=-1, configuration_ack=-1, command=0x3, signed_command=-1,
        chat=0x3, chat_session=-1, settings=0x5, server_payload=0xb),

    490: dict(  # 1.14.3
        known_packs=-1, join=0x25, respawn=0x3a, position=0x35,
        chunk=0x21, chunk_bulk=-1, keepalive=0x20, disconnect=0x1a,
        start_configuration=-1, system_chat=0xe, border=0x3d, payload=0x18,
        commands=0x11, batch_finished=-1, keepalive_reply=0xf, loaded=None,
        batch_reply=-1, configuration_ack=-1, command=0x3, signed_command=-1,
        chat=0x3, chat_session=-1, settings=0x5, server_payload=0xb),

    498: dict(  # 1.14.4
        known_packs=-1, join=0x25, respawn=0x3a, position=0x35,
        chunk=0x21, chunk_bulk=-1, keepalive=0x20, disconnect=0x1a,
        start_configuration=-1, system_chat=0xe, border=0x3d, payload=0x18,
        commands=0x11, batch_finished=-1, keepalive_reply=0xf, loaded=None,
        batch_reply=-1, configuration_ack=-1, command=0x3, signed_command=-1,
        chat=0x3, chat_session=-1, settings=0x5, server_payload=0xb),

    573: dict(  # 1.15
        known_packs=-1, join=0x26, respawn=0x3b, position=0x36,
        chunk=0x22, chunk_bulk=-1, keepalive=0x21, disconnect=0x1b,
        start_configuration=-1, system_chat=0xf, border=0x3e, payload=0x19,
        commands=0x12, batch_finished=-1, keepalive_reply=0xf, loaded=None,
        batch_reply=-1, configuration_ack=-1, command=0x3, signed_command=-1,
        chat=0x3, chat_session=-1, settings=0x5, server_payload=0xb),

    575: dict(  # 1.15.1
        known_packs=-1, join=0x26, respawn=0x3b, position=0x36,
        chunk=0x22, chunk_bulk=-1, keepalive=0x21, disconnect=0x1b,
        start_configuration=-1, system_chat=0xf, border=0x3e, payload=0x19,
        commands=0x12, batch_finished=-1, keepalive_reply=0xf, loaded=None,
        batch_reply=-1, configuration_ack=-1, command=0x3, signed_command=-1,
        chat=0x3, chat_session=-1, settings=0x5, server_payload=0xb),

    578: dict(  # 1.15.2
        known_packs=-1, join=0x26, respawn=0x3b, position=0x36,
        chunk=0x22, chunk_bulk=-1, keepalive=0x21, disconnect=0x1b,
        start_configuration=-1, system_chat=0xf, border=0x3e, payload=0x19,
        commands=0x12, batch_finished=-1, keepalive_reply=0xf, loaded=None,
        batch_reply=-1, configuration_ack=-1, command=0x3, signed_command=-1,
        chat=0x3, chat_session=-1, settings=0x5, server_payload=0xb),

    735: dict(  # 1.16
        known_packs=-1, join=0x25, respawn=0x3a, position=0x35,
        chunk=0x21, chunk_bulk=-1, keepalive=0x20, disconnect=0x1a,
        start_configuration=-1, system_chat=0xe, border=0x3d, payload=0x18,
        commands=0x11, batch_finished=-1, keepalive_reply=0x10, loaded=None,
        batch_reply=-1, configuration_ack=-1, command=0x3, signed_command=-1,
        chat=0x3, chat_session=-1, settings=0x5, server_payload=0xb),

    736: dict(  # 1.16.1
        known_packs=-1, join=0x25, respawn=0x3a, position=0x35,
        chunk=0x21, chunk_bulk=-1, keepalive=0x20, disconnect=0x1a,
        start_configuration=-1, system_chat=0xe, border=0x3d, payload=0x18,
        commands=0x11, batch_finished=-1, keepalive_reply=0x10, loaded=None,
        batch_reply=-1, configuration_ack=-1, command=0x3, signed_command=-1,
        chat=0x3, chat_session=-1, settings=0x5, server_payload=0xb),

    751: dict(  # 1.16.2
        known_packs=-1, join=0x24, respawn=0x39, position=0x34,
        chunk=0x20, chunk_bulk=-1, keepalive=0x1f, disconnect=0x19,
        start_configuration=-1, system_chat=0xe, border=0x3d, payload=0x17,
        commands=0x10, batch_finished=-1, keepalive_reply=0x10, loaded=None,
        batch_reply=-1, configuration_ack=-1, command=0x3, signed_command=-1,
        chat=0x3, chat_session=-1, settings=0x5, server_payload=0xb),

    753: dict(  # 1.16.3
        known_packs=-1, join=0x24, respawn=0x39, position=0x34,
        chunk=0x20, chunk_bulk=-1, keepalive=0x1f, disconnect=0x19,
        start_configuration=-1, system_chat=0xe, border=0x3d, payload=0x17,
        commands=0x10, batch_finished=-1, keepalive_reply=0x10, loaded=None,
        batch_reply=-1, configuration_ack=-1, command=0x3, signed_command=-1,
        chat=0x3, chat_session=-1, settings=0x5, server_payload=0xb),

    754: dict(  # 1.16.5
        known_packs=-1, join=0x24, respawn=0x39, position=0x34,
        chunk=0x20, chunk_bulk=-1, keepalive=0x1f, disconnect=0x19,
        start_configuration=-1, system_chat=0xe, border=0x3d, payload=0x17,
        commands=0x10, batch_finished=-1, keepalive_reply=0x10, loaded=None,
        batch_reply=-1, configuration_ack=-1, command=0x3, signed_command=-1,
        chat=0x3, chat_session=-1, settings=0x5, server_payload=0xb),

    755: dict(  # 1.17
        known_packs=-1, join=0x26, respawn=0x3d, position=0x38,
        chunk=0x22, chunk_bulk=-1, keepalive=0x21, disconnect=0x1a,
        start_configuration=-1, system_chat=0xf, border=0x20, payload=0x18,
        commands=0x12, batch_finished=-1, keepalive_reply=0xf, loaded=None,
        batch_reply=-1, configuration_ack=-1, command=0x3, signed_command=-1,
        chat=0x3, chat_session=-1, settings=0x5, server_payload=0xa),

    756: dict(  # 1.17.1
        known_packs=-1, join=0x26, respawn=0x3d, position=0x38,
        chunk=0x22, chunk_bulk=-1, keepalive=0x21, disconnect=0x1a,
        start_configuration=-1, system_chat=0xf, border=0x20, payload=0x18,
        commands=0x12, batch_finished=-1, keepalive_reply=0xf, loaded=None,
        batch_reply=-1, configuration_ack=-1, command=0x3, signed_command=-1,
        chat=0x3, chat_session=-1, settings=0x5, server_payload=0xa),

    757: dict(  # 1.18.1
        known_packs=-1, join=0x26, respawn=0x3d, position=0x38,
        chunk=0x22, chunk_bulk=-1, keepalive=0x21, disconnect=0x1a,
        start_configuration=-1, system_chat=0xf, border=0x20, payload=0x18,
        commands=0x12, batch_finished=-1, keepalive_reply=0xf, loaded=None,
        batch_reply=-1, configuration_ack=-1, command=0x3, signed_command=-1,
        chat=0x3, chat_session=-1, settings=0x5, server_payload=0xa),

    758: dict(  # 1.18.2
        known_packs=-1, join=0x26, respawn=0x3d, position=0x38,
        chunk=0x22, chunk_bulk=-1, keepalive=0x21, disconnect=0x1a,
        start_configuration=-1, system_chat=0xf, border=0x20, payload=0x18,
        commands=0x12, batch_finished=-1, keepalive_reply=0xf, loaded=None,
        batch_reply=-1, configuration_ack=-1, command=0x3, signed_command=-1,
        chat=0x3, chat_session=-1, settings=0x5, server_payload=0xa),

    759: dict(  # 1.19
        known_packs=-1, join=0x23, respawn=0x3b, position=0x36,
        chunk=0x1f, chunk_bulk=-1, keepalive=0x1e, disconnect=0x17,
        start_configuration=-1, system_chat=0x5f, border=0x1d, payload=0x15,
        commands=0xf, batch_finished=-1, keepalive_reply=0x11, loaded=None,
        batch_reply=-1, configuration_ack=-1, command=0x3, signed_command=0x3,
        chat=0x4, chat_session=-1, settings=0x7, server_payload=0xc),

    760: dict(  # 1.19.2
        known_packs=-1, join=0x25, respawn=0x3e, position=0x39,
        chunk=0x21, chunk_bulk=-1, keepalive=0x20, disconnect=0x19,
        start_configuration=-1, system_chat=0x62, border=0x1f, payload=0x16,
        commands=0xf, batch_finished=-1, keepalive_reply=0x12, loaded=None,
        batch_reply=-1, configuration_ack=-1, command=0x4, signed_command=0x4,
        chat=0x5, chat_session=-1, settings=0x8, server_payload=0xd),

    761: dict(  # 1.19.3
        known_packs=-1, join=0x24, respawn=0x3d, position=0x38,
        chunk=0x20, chunk_bulk=-1, keepalive=0x1f, disconnect=0x17,
        start_configuration=-1, system_chat=0x60, border=0x1e, payload=0x15,
        commands=0xe, batch_finished=-1, keepalive_reply=0x11, loaded=None,
        batch_reply=-1, configuration_ack=-1, command=0x4, signed_command=0x4,
        chat=0x5, chat_session=0x20, settings=0x7, server_payload=0xc),

    762: dict(  # 1.19.4
        known_packs=-1, join=0x28, respawn=0x41, position=0x3c,
        chunk=0x24, chunk_bulk=-1, keepalive=0x23, disconnect=0x1a,
        start_configuration=-1, system_chat=0x64, border=0x22, payload=0x17,
        commands=0x10, batch_finished=-1, keepalive_reply=0x12, loaded=None,
        batch_reply=-1, configuration_ack=-1, command=0x4, signed_command=0x4,
        chat=0x5, chat_session=0x6, settings=0x8, server_payload=0xd),

    763: dict(  # 1.20.1
        known_packs=-1, join=0x28, respawn=0x41, position=0x3c,
        chunk=0x24, chunk_bulk=-1, keepalive=0x23, disconnect=0x1a,
        start_configuration=-1, system_chat=0x64, border=0x22, payload=0x17,
        commands=0x10, batch_finished=-1, keepalive_reply=0x12, loaded=None,
        batch_reply=-1, configuration_ack=-1, command=0x4, signed_command=0x4,
        chat=0x5, chat_session=0x6, settings=0x8, server_payload=0xd),

    764: dict(  # 1.20.2
        known_packs=-1, join=0x29, respawn=0x43, position=0x3e,
        chunk=0x25, chunk_bulk=-1, keepalive=0x24, disconnect=0x1b,
        start_configuration=0x65, system_chat=0x67, border=0x23, payload=0x18,
        commands=0x11, batch_finished=0xc, keepalive_reply=0x14, loaded=None,
        batch_reply=0x7, configuration_ack=0xb, command=0x4, signed_command=0x4,
        chat=0x5, chat_session=0x6, settings=0x9, server_payload=0xf),

    765: dict(  # 1.20.4
        known_packs=-1, join=0x29, respawn=0x45, position=0x3e,
        chunk=0x25, chunk_bulk=-1, keepalive=0x24, disconnect=0x1b,
        start_configuration=0x67, system_chat=0x69, border=0x23, payload=0x18,
        commands=0x11, batch_finished=0xc, keepalive_reply=0x15, loaded=None,
        batch_reply=0x7, configuration_ack=0xb, command=0x4, signed_command=0x4,
        chat=0x5, chat_session=0x6, settings=0x9, server_payload=0x10),

    766: dict(  # 1.20.6
        known_packs=0xe, join=0x2b, respawn=0x47, position=0x40,
        chunk=0x27, chunk_bulk=-1, keepalive=0x26, disconnect=0x1d,
        start_configuration=0x69, system_chat=0x6c, border=0x25, payload=0x19,
        commands=0x11, batch_finished=0xc, keepalive_reply=0x18, loaded=None,
        batch_reply=0x8, configuration_ack=0xc, command=0x4, signed_command=0x5,
        chat=0x6, chat_session=0x7, settings=0xa, server_payload=0x12),
    767: dict(  # 1.21.1
        known_packs=0x0E, join=0x2B, position=0x40, chunk=0x27,
        keepalive=0x26, disconnect=0x1D, start_configuration=0x69, system_chat=0x6C,
        border=0x25, payload=0x19, commands=0x11, batch_finished=0x0C,
        keepalive_reply=0x18, loaded=None, batch_reply=0x08, configuration_ack=0x0C,
        command=0x04, signed_command=0x05, chat=0x06, chat_session=0x07),
    768: dict(  # 1.21.3
        known_packs=0x0E, join=0x2C, position=0x42, chunk=0x28,
        keepalive=0x27, disconnect=0x1D, start_configuration=0x70, system_chat=0x73,
        border=0x26, payload=0x19, commands=0x11, batch_finished=0x0C,
        keepalive_reply=0x1A, loaded=None, batch_reply=0x09, configuration_ack=0x0E,
        command=0x05, signed_command=0x06, chat=0x07, chat_session=0x08),
    769: dict(  # 1.21.4
        known_packs=0x0E, join=0x2C, position=0x42, chunk=0x28,
        keepalive=0x27, disconnect=0x1D, start_configuration=0x70, system_chat=0x73,
        border=0x26, payload=0x19, commands=0x11, batch_finished=0x0C,
        keepalive_reply=0x1A, loaded=0x2A, batch_reply=0x09, configuration_ack=0x0E,
        command=0x05, signed_command=0x06, chat=0x07, chat_session=0x08),
    770: dict(  # 1.21.5
        known_packs=0x0E, join=0x2B, position=0x41, chunk=0x27,
        keepalive=0x26, disconnect=0x1C, start_configuration=0x6F, system_chat=0x72,
        border=0x25, payload=0x18, commands=0x10, batch_finished=0x0B,
        keepalive_reply=0x1A, loaded=0x2A, batch_reply=0x09, configuration_ack=0x0E,
        command=0x05, signed_command=0x06, chat=0x07, chat_session=0x08),
    771: dict(  # 1.21.6
        known_packs=0x0E, join=0x2B, position=0x41, chunk=0x27,
        keepalive=0x26, disconnect=0x1C, start_configuration=0x6F, system_chat=0x72,
        border=0x25, payload=0x18, commands=0x10, batch_finished=0x0B,
        keepalive_reply=0x1B, loaded=0x2B, batch_reply=0x0A, configuration_ack=0x0F,
        command=0x06, signed_command=0x07, chat=0x08, chat_session=0x09),
    772: dict(  # 1.21.8
        known_packs=0x0E, join=0x2B, position=0x41, chunk=0x27,
        keepalive=0x26, disconnect=0x1C, start_configuration=0x6F, system_chat=0x72,
        border=0x25, payload=0x18, commands=0x10, batch_finished=0x0B,
        keepalive_reply=0x1B, loaded=0x2B, batch_reply=0x0A, configuration_ack=0x0F,
        command=0x06, signed_command=0x07, chat=0x08, chat_session=0x09),
    773: dict(  # 1.21.9
        known_packs=0x0E, join=0x30, position=0x46, chunk=0x2C,
        keepalive=0x2B, disconnect=0x20, start_configuration=0x74, system_chat=0x77,
        border=0x2A, payload=0x18, commands=0x10, batch_finished=0x0B,
        keepalive_reply=0x1B, loaded=0x2B, batch_reply=0x0A, configuration_ack=0x0F,
        command=0x06, signed_command=0x07, chat=0x08, chat_session=0x09),
    774: dict(  # 1.21.11
        known_packs=0x0E, join=0x30, position=0x46, chunk=0x2C,
        keepalive=0x2B, disconnect=0x20, start_configuration=0x74, system_chat=0x77,
        border=0x2A, payload=0x18, commands=0x10, batch_finished=0x0B,
        keepalive_reply=0x1B, loaded=0x2B, batch_reply=0x0A, configuration_ack=0x0F,
        command=0x06, signed_command=0x07, chat=0x08, chat_session=0x09),
    775: dict(  # 26.1
        known_packs=0x0E, join=0x31, position=0x48, chunk=0x2D,
        keepalive=0x2C, disconnect=0x20, start_configuration=0x76, system_chat=0x79,
        border=0x2B, payload=0x18, commands=0x10, batch_finished=0x0B,
        keepalive_reply=0x1C, loaded=0x2C, batch_reply=0x0B, configuration_ack=0x10,
        command=0x07, signed_command=0x08, chat=0x09, chat_session=0x0A),
    776: dict(  # 26.2
        known_packs=0x0E, join=0x31, position=0x48, chunk=0x2D,
        keepalive=0x2C, disconnect=0x20, start_configuration=0x76, system_chat=0x79,
        border=0x2B, payload=0x18, commands=0x10, batch_finished=0x0B,
        keepalive_reply=0x1C, loaded=0x2C, batch_reply=0x0B, configuration_ack=0x10,
        command=0x07, signed_command=0x08, chat=0x09, chat_session=0x0A),
    777: dict(  # 26.3
        known_packs=0x0F, join=0x32, position=0x49, chunk=0x2E,
        keepalive=0x2D, disconnect=0x20, start_configuration=0x78, system_chat=0x7C,
        border=0x2C, payload=0x18, commands=0x10, batch_finished=0x0B,
        keepalive_reply=0x1C, loaded=0x2C, batch_reply=0x0B, configuration_ack=0x10,
        command=0x07, signed_command=0x08, chat=0x09, chat_session=0x0A),
}


def varint(value):
    if not 0 <= value <= 0xFFFFFFFF:
        raise ValueError("VarInt must be an unsigned 32-bit value")
    result = bytearray()
    while value > 127:
        result.append((value & 127) | 128)
        value >>= 7
    result.append(value)
    return bytes(result)


def read_varint(stream):
    result = 0
    for shift in range(0, 35, 7):
        byte = stream.read(1)
        if not byte:
            raise EOFError("connection closed mid-packet")
        result |= (byte[0] & 127) << shift
        if shift == 28 and byte[0] > 15:
            raise ValueError("invalid VarInt")
        if not byte[0] & 128:
            return result
    raise ValueError("invalid VarInt")


def string(value):
    encoded = value.encode()
    return varint(len(encoded)) + encoded


def login_start(name, player_id, protocol):
    data = string(name)
    if protocol in (759, 760):
        data += b"\0"  # No profile key on offline test clients.
    if protocol >= 760:
        if protocol < 764:
            data += b"\1"
        data += player_id.bytes
    return data


def client_settings(protocol):
    settings = string("en_us") + bytes([2, 0, 1, 127])
    if protocol >= 107:
        settings += b"\1"  # Main hand.
    if protocol >= 755:
        settings += b"\0"  # Text filtering.
    if protocol >= 757:
        settings += b"\1"  # Server listing.
    if protocol >= 768:
        settings += b"\2"  # Particle status.
    return settings


def empty_chat_update(protocol):
    if protocol < 759:
        return b""
    if protocol == 759:
        return bytes(2)  # Empty signature/list, no signed preview.
    if protocol == 760:
        return bytes(4)  # Plus empty last-seen and rejected-message lists.
    return b"\0" * 5 + (b"\1" if protocol >= 770 else b"")


def teleport_acknowledgement(body, protocol):
    data = io.BytesIO(body)
    if protocol < 768:
        assert len(data.read(33)) == 33  # Position, rotation and relative flags.
    reply = varint(read_varint(data))
    if protocol == 777:
        # 26.3 also requires the accepted position and rotation. These fixtures
        # send an absolute spawn teleport; fail if that assumption changes.
        position = data.read(24)
        data.read(24)  # Velocity.
        rotation = data.read(8)
        assert data.read(4) == b"\0" * 4, "relative spawn teleport"
        reply += position + rotation
    return reply


class Client:
    def __init__(self, port, state, protocol=774, hostname="localhost"):
        self.socket = socket.create_connection(("127.0.0.1", port), timeout=20)
        self.socket.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.reader = self.socket.makefile("rb")
        self.threshold = None
        self.compressed_packets = 0
        self.sent_compressed_packets = 0
        self.deadline = time.monotonic() + 120
        self.send(0, varint(protocol) + string(hostname) + struct.pack(">H", port) + varint(state))

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.reader.close()
        self.socket.close()

    def send(self, packet_id, body=b""):
        packet = varint(packet_id) + body
        if self.threshold is not None:
            self.sent_compressed_packets += len(packet) >= self.threshold
            packet = (varint(len(packet)) + zlib.compress(packet)
                      if len(packet) >= self.threshold else b"\0" + packet)
        self.socket.sendall(varint(len(packet)) + packet)

    def receive(self):
        remaining = self.deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("Minecraft session deadline exceeded")
        self.socket.settimeout(min(getattr(self, "read_timeout", 20), remaining))
        length = read_varint(self.reader)
        assert 0 < length <= 8 * 1024 * 1024, length
        data = self.reader.read(length)
        if len(data) != length:
            raise EOFError("truncated packet")
        if self.threshold is not None:
            framed = io.BytesIO(data)
            uncompressed = read_varint(framed)
            data = framed.read()
            if uncompressed:
                assert uncompressed <= 8 * 1024 * 1024, uncompressed
                data = zlib.decompress(data)
                assert len(data) == uncompressed
                self.compressed_packets += 1
        packet = io.BytesIO(data)
        packet_id = read_varint(packet)
        return packet_id, packet.read()


def status(port, protocol=774, hostname="localhost", ping=123456789012345):
    with Client(port, 1, protocol, hostname) as client:
        client.send(0)
        packet_id, body = client.receive()
        assert packet_id == 0
        data = io.BytesIO(body)
        result = json.loads(data.read(read_varint(data)))
        payload = struct.pack(">q", ping)
        client.send(1, payload)
        assert client.receive() == (1, payload), "ping payload changed"
        return result


def play(port, name, protocol=774, compression=True, pumpkin=False, hostname="localhost",
         observe=None):
    packets = PROTOCOLS[protocol]
    # An offline-mode fixture avoids needing an actual Microsoft account/token.
    digest = hashlib.md5(f"OfflinePlayer:{name}".encode()).digest()
    player_id = uuid.UUID(bytes=digest, version=3)
    # Pumpkin's pinned net::offline_uuid uses the first 16 SHA-256 bytes.
    if pumpkin:
        player_id = uuid.UUID(bytes=hashlib.sha256(name.encode()).digest()[:16])
    with Client(port, 2, protocol, hostname) as client:
        if observe is not None:
            client.deadline = time.monotonic() + 900
            observe(client, {})
        client.send(0, login_start(name, player_id, protocol))
        while True:
            packet_id, body = client.receive()
            if packet_id == 3:
                client.threshold = read_varint(io.BytesIO(body))
            elif packet_id == 2:
                profile = io.BytesIO(body)
                if protocol < 735:
                    assert profile.read(read_varint(profile)).decode() == str(player_id)
                else:
                    assert profile.read(16) == player_id.bytes, "login UUID changed"
                assert profile.read(read_varint(profile)).decode() == name
                break
            else:
                raise AssertionError(f"unexpected login packet {packet_id}: {body[:200]!r}")
        def options(configuration=False):
            settings_id = 0 if configuration else packets["settings"]
            payload_id = (2 if protocol >= 766 else 1) if configuration else packets["server_payload"]
            client.send(settings_id, client_settings(protocol))
            brand = "minecraft:brand" if protocol >= 393 else "MC|Brand"
            client.send(payload_id, string(brand) + string("rift-test"))
            channel = "rift:compression_test" if protocol >= 393 else "RiftTest"
            client.send(payload_id, string(channel) + b"x" * 512)

        if protocol >= 764:
            client.send(3)
            options(configuration=True)
            finish = 3 if protocol >= 766 else 2
            while True:
                packet_id, body = client.receive()
                if packet_id == packets["known_packs"]:
                    client.send(7, b"\0")
                elif packet_id in ((4, 5) if protocol >= 766 else (3, 4)):
                    client.send(packet_id, body)
                elif packet_id == finish:
                    client.send(finish)
                    break
                elif packet_id == finish - 1:
                    raise AssertionError(f"configuration disconnect: {body[:200]!r}")
        joined = positioned = False
        keepalives = 0
        chunks = 0
        teleports = 0
        deadline = time.monotonic() + (900 if observe is not None else 60)
        while time.monotonic() < deadline:
            packet_id, body = client.receive()
            if packet_id == packets["join"]:
                if protocol < 764 and not joined:
                    options()
                joined = True
            elif packet_id == packets["position"]:
                if protocol == 47:
                    client.send(6, body[:32] + b"\1")
                else:
                    client.send(0, teleport_acknowledgement(body, protocol))
                if packets["loaded"] is not None:
                    client.send(packets["loaded"])
                positioned = True
                teleports += 1
            elif packet_id in (packets["chunk"], packets.get("chunk_bulk", -1)):
                chunks += 1
            elif packet_id == packets["batch_finished"]:
                client.send(packets["batch_reply"], struct.pack(">f", 10.0))
            elif packet_id == packets["keepalive"]:
                client.send(packets["keepalive_reply"], body)
                keepalives += 1
                # A second keepalive demonstrates that the server accepted our
                # first reply and kept the session alive.
                if joined and positioned and chunks and keepalives >= 2:
                    assert (client.threshold is not None) == compression
                    assert (client.compressed_packets > 0) == compression
                    assert (client.sent_compressed_packets > 0) == compression
                    if observe is None:
                        return chunks, client.compressed_packets
            elif packet_id == packets["disconnect"]:
                raise AssertionError(f"play disconnect: {body[:200]!r}")
            if observe is not None:
                observe(client, dict(joined=joined, teleports=teleports, chunks=chunks,
                                     keepalives=keepalives, compressed=client.compressed_packets,
                                     sent_compressed=client.sent_compressed_packets))
        raise AssertionError(f"incomplete play: joined={joined}, positioned={positioned}, chunks={chunks}")


def unused_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


@contextmanager
def process(command, directory, log_name, server=False, env=None):
    with (directory / log_name).open("w") as log:
        proc = subprocess.Popen(command, cwd=directory, stdin=subprocess.PIPE,
                                stdout=log, stderr=subprocess.STDOUT, text=True, env=env)
        try:
            yield proc
        finally:
            if proc.poll() is None:
                if server:
                    try:
                        proc.stdin.write("stop\n")
                        proc.stdin.flush()
                    except BrokenPipeError:
                        pass
                else:
                    proc.terminate()
                try:
                    proc.wait(timeout=30)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait()
            proc.stdin.close()


def wait_ready(proc, ready, log, timeout=180):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            raise RuntimeError(f"process exited ({proc.returncode}): {log}\n{log.read_text()[-4000:]}")
        if ready():
            return
        time.sleep(0.1)
    raise TimeoutError(f"startup timed out: {log}\n{log.read_text()[-4000:]}")


def download(name):
    fixture = SERVERS[name]
    directory = CACHE / "downloads"
    directory.mkdir(parents=True, exist_ok=True)
    artifact = directory / fixture["filename"]
    if not artifact.exists():
        print(f"Downloading {name}...", flush=True)
        request = urllib.request.Request(fixture["url"], headers={
            "User-Agent": "rift-integration-tests/0.1.0 (https://github.com/Jotrorox/rift)"
        })
        # Atomic installation prevents interrupted downloads poisoning the cache.
        with tempfile.TemporaryDirectory(dir=directory) as temporary:
            pending = Path(temporary) / "download"
            with pending.open("wb") as destination:
                with urllib.request.urlopen(request, timeout=120) as response:
                    while block := response.read(1024 * 1024):
                        destination.write(block)
            verify_checksum(pending, fixture)
            pending.replace(artifact)
    verify_checksum(artifact, fixture)
    if name == "pumpkin":
        artifact.chmod(0o755)
    return artifact


def verify_checksum(path, fixture):
    with path.open("rb") as source:
        actual = hashlib.file_digest(source, fixture["algorithm"]).hexdigest()
    if actual != fixture["checksum"]:
        raise ValueError(f"checksum mismatch: {path}: {actual}")


def configure_server(name, directory, backend, compression, motd=None, online=False):
    artifact = download(name)
    motd = motd or f"rift-{name}-test"
    if name == "pumpkin":
        if online:
            raise ValueError("the manual authenticated fixture uses vanilla or Paper")
        (directory / "pumpkin.toml").write_text(
            'seed = "12345"\ndefault_gamemode = "Creative"\n'
            'default_difficulty = "Peaceful"\nallow_nether = false\nallow_end = false\n'
            '[telemetry]\nenabled = false\n[plugins]\nenabled = false\n'
            '[commands]\nuse_console = true\nuse_tty = false\n'
            '[networking.java]\nenabled = true\n'
            f'address = "127.0.0.1:{backend}"\nmotd = "{motd}"\n'
            'online_mode = false\nencryption = false\nview_distance = 2\n'
            'simulation_distance = 2\nmax_players = 20\nkeep_alive_time = 3\n'
            '[networking.java.authentication]\nenabled = false\n'
            f'[networking.java.compression]\nenabled = {str(compression).lower()}\n'
            'threshold = 256\n[networking.bedrock]\nenabled = false\n'
            '[networking.bedrock.nethernet]\nenabled = false\n'
        )
        return [str(artifact)]

    (directory / "eula.txt").write_text("eula=true\n")
    (directory / "server.properties").write_text(
        f"server-ip=127.0.0.1\nserver-port={backend}\nmotd={motd}\n"
        f"online-mode={str(online).lower()}\nenforce-secure-profile={str(online).lower()}\n"
        "prevent-proxy-connections=false\nwhite-list=false\nenforce-whitelist=false\n"
        f"network-compression-threshold={256 if compression else -1}\n"
        "gamemode=creative\nforce-gamemode=true\ndifficulty=peaceful\n"
        "view-distance=2\nsimulation-distance=2\nmax-players=20\nlevel-type=minecraft:flat\n"
        'generator-settings={"layers":[{"block":"minecraft:bedrock","height":1},'
        '{"block":"minecraft:dirt","height":2},{"block":"minecraft:grass_block","height":1}],'
        '"biome":"minecraft:plains"}\n'
        "generate-structures=false\nspawn-protection=0\nmax-tick-time=-1\n"
        "pause-when-empty-seconds=0\n"
    )
    java = os.environ.get(f"RIFT_JAVA_{SERVERS[name]['java']}", "java")
    if SERVERS[name]["protocol"] < 759:
        properties = directory / "server.properties"
        properties.write_text(properties.read_text().replace("level-type=minecraft:flat", "level-type=flat"))
    if SERVERS[name]["protocol"] < 477:
        properties = directory / "server.properties"
        properties.write_text(properties.read_text().replace("level-type=flat", "level-type=FLAT")
                              .replace("gamemode=creative", "gamemode=1")
                              .replace("difficulty=peaceful", "difficulty=0"))
    return [java, "-XX:ActiveProcessorCount=2", "-XX:+UseSerialGC", "-Xms256M", "-Xmx768M",
            "-jar", str(artifact), "nogui"]


def status_ready(port, protocol):
    try:
        status(port, protocol)
        return True
    except (OSError, EOFError):
        return False


def test_hostname_routing(name, binary, directory, compression, first_backend):
    """Two real servers with distinct MOTDs, addressed through one proxy port."""
    protocol = SERVERS[name]["protocol"]
    second_backend, frontend = unused_port(), unused_port()
    while second_backend == frontend:
        frontend = unused_port()
    second_dir = directory / "routing-second"
    second_dir.mkdir()
    second_motd = f"rift-{name}-second-test"
    command = configure_server(name, second_dir, second_backend, compression, second_motd)
    with process(command, second_dir, "server.log", server=True) as server:
        wait_ready(server, lambda: status_ready(second_backend, protocol), second_dir / "server.log")
        config_path = directory / "routing.lua"
        config_path.write_text(f"""return {{
            listeners = {{public = '127.0.0.1:{frontend}'}},
            backends = {{first = '127.0.0.1:{first_backend}', second = 'localhost:{second_backend}'}},
            routes = {{public = {{['survival.example.test']='first', ['creative.example.test']='second',
                                 ['*.games.example.test']='second', ['*']='first'}}}},
            status_cache = {{}},
        }}""")
        with process([str(binary), "--config", str(config_path)],
                     directory, "routing-proxy.log") as proxy:
            wait_ready(proxy, lambda: "rift:" in (directory / "routing-proxy.log").read_text(),
                       directory / "routing-proxy.log")
            expected = {
                "survival.example.test": status(first_backend, protocol),
                "creative.example.test": status(second_backend, protocol),
            }
            assert expected["survival.example.test"]["description"] != expected["creative.example.test"]["description"]
            hosts = list(expected) * 16
            with ThreadPoolExecutor(max_workers=16) as pool:
                replies = list(pool.map(lambda host: status(frontend, protocol, host), hosts))
            for host, reply in zip(hosts, replies):
                assert reply == expected[host], (host, reply)
            assert status(frontend, protocol, "pvp.games.example.test") == expected["creative.example.test"]
            assert status(frontend, protocol, "unmatched.example.test") == expected["survival.example.test"]
            for host in expected:
                play(frontend, "RiftR" + uuid.uuid4().hex[:8], protocol, compression,
                     name == "pumpkin", hostname=host)
            assert proxy.poll() is None
            print(f"PASS {name}: two domains, two servers, one port; DNS, wildcard, default, concurrent status and gameplay", flush=True)
    assert server.returncode == 0, f"routing server shutdown failed: {server.returncode}"


def test_server(name, binary, directory, compression):
    fixture = SERVERS[name]
    protocol = fixture["protocol"]
    backend, frontend = unused_port(), unused_port()
    while frontend == backend:
        frontend = unused_port()
    command = configure_server(name, directory, backend, compression)
    proxy_log = directory / "proxy.log"
    server_log = directory / "server.log"
    config_path = directory / "proxy.lua"
    config_path.write_text(f"""return {{
        listeners = {{public = '127.0.0.1:{frontend}'}},
        backends = {{main = '127.0.0.1:{backend}'}}, routes = {{public = 'main'}}, status_cache = {{}},
    }}""")
    with process([str(binary), "--config", str(config_path)],
                 directory, "proxy.log") as proxy:
        wait_ready(proxy, lambda: "rift:" in proxy_log.read_text(), proxy_log)
        # An unavailable backend returns a readable login disconnect; status remains available.
        with Client(frontend, 2, protocol) as client:
            packet_id, body = client.receive()
            assert packet_id == 0 and b"unavailable" in body
        print(f"Starting {name} {fixture['version']} (compression={compression})...", flush=True)
        with process(command, directory, "server.log", server=True) as server:
            wait_ready(server, lambda: status_ready(backend, protocol), server_log)
            direct, proxied = status(backend, protocol), status(frontend, protocol)
            assert direct == proxied, (direct, proxied)
            assert direct["version"]["protocol"] == protocol
            assert f"rift-{name}-test" in json.dumps(proxied["description"])
            with ThreadPoolExecutor(max_workers=16) as pool:
                responses = list(pool.map(lambda port: status(port, protocol), [frontend] * 64))
            assert all(item["version"] == direct["version"] for item in responses)
            print(f"PASS {name}: backend recovery, status equality, ping, 64 status requests (16 concurrent)", flush=True)
            # Fresh players avoid saved health/location affecting repeat runs.
            for port, prefix in [(backend, "RiftD"), (frontend, "RiftP")]:
                player = prefix + uuid.uuid4().hex[:8]
                chunks, compressed = play(port, player, protocol, compression, name == "pumpkin")
                route = "direct" if port == backend else "proxied"
                print(f"PASS {name} {route}: login, configuration, {chunks} chunks, two keepalives ({compressed} compressed packets)", flush=True)
            test_hostname_routing(name, binary, directory, compression, backend)
            assert proxy.poll() is None
        assert server.returncode == 0, f"server shutdown failed: {server.returncode}"
        assert proxy.poll() is None
        with Client(frontend, 2, protocol) as client:
            packet_id, body = client.receive()
            assert packet_id == 0 and b"unavailable" in body
        print(f"PASS {name}: backend shutdown handled", flush=True)


def run_server(name, binary, compression, rounds=12):
    start = time.monotonic()
    runs = CACHE / "runs"
    runs.mkdir(parents=True, exist_ok=True)
    directory = Path(tempfile.mkdtemp(prefix=f"{name}-", dir=runs))
    result = dict(server=name, version=SERVERS[name]["version"], compression=compression,
                  logs=str(directory), passed=False)
    try:
        test_server(name, binary, directory, compression)
        if platform.system() == "Linux":
            from operations import test_operations
            result["operations"] = {}
            test_operations(name, binary, directory / "operations", compression,
                            rounds, result["operations"])
        else:
            result["operations"] = {"skipped": "requires Linux signals and /proc resource accounting"}
        result["passed"] = True
    except Exception:
        result["error"] = traceback.format_exc()
        print(result["error"], flush=True)
        for log in directory.rglob("*.log"):
            print(f"--- {log} ---\n{log.read_text(errors='replace')[-6000:]}", flush=True)
    result["seconds"] = round(time.monotonic() - start, 2)
    (directory / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--accept-eula", action="store_true",
                        help="accept https://aka.ms/MinecraftEULA for the local test servers")
    parser.add_argument("--server", action="append", choices=SERVERS,
                        help="server to test; repeat to select several (default: all)")
    parser.add_argument("--jobs", type=int, choices=range(1, 4), default=1,
                        help="number of server processes to test in parallel")
    parser.add_argument("--compression", choices=["enabled", "disabled"], default="enabled")
    parser.add_argument("--binary", type=Path, help="test this Rift binary instead of building")
    parser.add_argument("--report", type=Path, help="write an aggregate JSON result, including failures")
    parser.add_argument("--operation-rounds", type=int, default=12,
                        help="reload/status stress rounds on Linux (minimum 4, default 12)")
    args = parser.parse_args()
    if not __debug__:
        parser.error("do not use python -O: integration assertions must be enabled")
    if not args.accept_eula:
        parser.error("--accept-eula is required to run the Minecraft servers")
    if args.operation_rounds < 4:
        parser.error("--operation-rounds must be at least 4")
    names = list(dict.fromkeys(args.server or SERVERS))
    if "pumpkin" in names and (platform.system() != "Linux" or platform.machine() != "x86_64"):
        parser.error("the pinned Pumpkin binary requires Linux x86_64; select --server vanilla --server paper elsewhere")
    CACHE.mkdir(parents=True, exist_ok=True)
    if args.binary:
        binary = args.binary.resolve(strict=True)
    else:
        subprocess.run(["cargo", "build", "--release", "--locked"], cwd=ROOT, check=True)
        binary = ROOT / "target" / "release" / ("rift.exe" if platform.system() == "Windows" else "rift")
    with ThreadPoolExecutor(max_workers=args.jobs) as pool:
        results = list(pool.map(lambda name: run_server(
            name, binary, args.compression == "enabled", args.operation_rounds), names))
    if args.report:
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(json.dumps(results, indent=2) + "\n")
    if not all(result["passed"] for result in results):
        raise SystemExit("Minecraft integration failed; see logs and result.json in target/minecraft/runs/")
    print("All Minecraft integration tests passed.", flush=True)


if __name__ == "__main__":
    main()
