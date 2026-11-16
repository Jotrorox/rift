# Status page

The `status` service is a separate, read-only website without a login. It
serves a page at `/`, JSON at `/status`, a badge at `/badge.svg` and, when
`status.metrics` is on, Prometheus text at `/metrics`. Set
`exposure = "public"` to make it a page you can link players to.

```lua
status = {
    listen = "0.0.0.0:9090",
    exposure = "public", -- Default "operator" keeps addresses and counters.
    backends = {
        survival1 = "Survival", -- Display name on the page and in JSON.
        arena = "Arena",        -- A service group: instances become "Arena 1", "Arena 2", …
        limbo = false,          -- Hidden from the status server.
    },
    cors = { "https://example.com" }, -- Or "*"; lets that site fetch /status.
    ui = {
        title = "Example Network",
        notice = "Maintenance Sunday 02:00 UTC",
        accent = "#ff6a00",
        refresh_ms = 10000,
        panels = { summary = true, backends = true, listeners = false, metrics = false },
        links = {
            { label = "Discord", href = "https://discord.gg/example" },
            { label = "Rules", href = "/ext/rules" },
        },
    },
},
```

[Web dashboard](http.md#configure-the-services) covers `enabled`, `listen`
and `metrics`.

## Choose what the site reveals

Anyone who can reach the status server can read it:

- With the default `status.exposure = "operator"`, status JSON includes
  backend and listener addresses, health eligibility, counters and service
  settings. Bind the server to a monitoring interface appropriate for that
  information.
- With `exposure = "public"`, it reveals only names, health and player
  counts.

Neither exposure includes Lua source, tokens, configuration filesystem paths
or managed lifecycle details.

> [!WARNING]
> Switch to `exposure = "public"` before you expose the status server to
> players.

Player counts and the badge, under either exposure, leave out players a
[visibility rule](permissions.md#visibility) hides. The `operator` exposure's
`metrics` counters and `/api/status` count everyone, as Prometheus does.

## Settings

| Field | Default | Meaning |
| --- | --- | --- |
| `exposure` | `"operator"` | `"public"` omits addresses, listeners, counters, the Rift version, the configuration revision and service settings from `/status`, and turns off `/metrics` unless `metrics = true` is set explicitly |
| `backends` | `{}` | Per backend or service group: a display name (1–64 characters) or `false` to hide it. Runtime instances are configured through their group. Hidden backends still appear on the authenticated `/api/status` and in the Lua `on_http` context |
| `cors` | none | Origins (`scheme://host[:port]`, no path) allowed to read `/status` from a browser, or `"*"` alone. Credentials are never allowed |
| `ui` | `true` | `false` disables the page; a table enables it and styles it |
| `ui.title` | `"Rift"` | Brand name in the header, the document title and the badge label (1–80 characters) |
| `ui.notice` | none | A banner under the verdict, such as planned maintenance (1–500 characters) |
| `ui.accent` | theme purple | `#rgb` or `#rrggbb`, applied to the theme's accent color |
| `ui.refresh_ms` | `5000` | Refresh interval, `1000`–`300000` |
| `ui.panels` | all `true` | Show or hide `summary`, `backends`, `listeners` and `metrics`. Public exposure always hides listeners and metrics |
| `ui.links` | none | Up to 8 footer links; `href` is an `http(s)` URL or a path starting with `/` |
| `ui.enabled` | `true` | Keep the table but disable the page |

All of these hot-reload, so `ui.notice` doubles as an incident banner: edit it
and reload.

Rift renders the title and notice into the HTML, for link previews and readers
without JavaScript. The page applies everything else from the `page` object
in `/status`, without loosening the strict Content Security Policy.

## Read the status JSON

Public `/status` has this shape. Operator exposure adds the fields it always
had, plus `address` per backend.

```json
{
  "exposure": "public",
  "uptime_seconds": 86400,
  "players": 42,
  "health_checks_enabled": true,
  "backends": [
    { "name": "survival1", "display": "Survival", "version": null, "healthy": true, "players": 30 }
  ],
  "page": { "title": "Example Network", "notice": null, "accent": "#ff6a00", "refresh_ms": 10000,
            "panels": { "summary": true, "backends": true, "listeners": false, "metrics": false },
            "links": [ { "label": "Discord", "href": "https://discord.gg/example" } ] },
  "services": { "status": { "metrics": false } }
}
```

- `players` counts everyone online, including players on hidden backends.
  Each backend's `players` counts players currently attached to it.
- A healthy backend may still be administratively draining and reject new
  attachments.
- `health_checks_enabled` reports the policy. With health checks disabled, an
  up value does not imply a successful probe.
- The [admin CLI](admin.md) and the `rift_backend_draining` and
  `rift_maintenance_mode` metrics expose effective maintenance state,
  including runtime overrides.

To render the data on your own website, list its origin in `cors` and fetch
`/status` from there.

## Embed the badge

`GET /badge.svg` returns a small SVG for READMEs and forum signatures: the
`ui.title` (or "players") and `N online`. It is served whenever the status
server runs, including with `ui = false`.

| Color | Meaning |
| --- | --- |
| Green | Every backend is healthy |
| Amber | Some backends are unhealthy |
| Blue | Health checks are off |
| Red, reading `offline` | Every visible backend is unhealthy |
