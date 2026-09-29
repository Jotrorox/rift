-- Run: rift check examples/modular/rift.lua
-- Start: rift --config examples/modular/rift.lua
-- Paths are relative to this file, regardless of the working directory.
require("config.network")
require("config.services")

-- Each plugin is a folder with init.lua and optional lua/ modules.
rift.plugin("greeting")
rift.plugin("echo")
