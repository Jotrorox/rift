"""Schemas for protocols minecraft-data does not cover yet (placeholder)."""

DERIVED = {776: "26.2", 777: "26.3"}


def derive(version):
    raise KeyError(f"no derived schema for {version} yet")
