// Facts shown on the landing page. Every number here comes from
// README.md or docs/performance.md; update them together.
import { escapeHtml } from './content.mjs';

export const SITE = {
  name: 'Rift',
  summary: 'A Minecraft Java Edition proxy written in Rust with embedded LuaJIT.',
  description:
    'Route hostnames through one port, authenticate players, switch backends and manage ' +
    'configuration through Lua or the bundled web dashboard. No Java or separate Lua installation needed.',
  releases: 'https://github.com/Jotrorox/rift/releases',
  repo: 'https://github.com/Jotrorox/rift',
  versions: { from: '1.8.9', to: '26.3', releases: 66, protocols: 50, onlineFrom: '1.19.3' },
  license: 'BSD-2-Clause',
};

export const QUICK_START = [
  { cmd: './rift init', note: 'Writes a commented rift.lua.' },
  { cmd: './rift check', note: 'Validates it without opening any ports.' },
  { cmd: './rift', note: 'Starts the proxy.' },
];

// Median values, 16 concurrent clients, from docs/performance.md.
export const PERF = {
  clients: 16,
  memory: { rift: 8.86, velocity: 354.40, unit: 'MiB', label: 'Peak memory' },
  loginCpu: { rift: 54.0, velocity: 208.12, unit: '%', label: 'CPU during login bursts' },
};

// The three headline features get a card each; the rest are listed below them.
export const FEATURES = [
  { key: 'routing', title: 'Hostname routing',
    text: 'One listener serves many servers. Exact names win, then the longest wildcard, then the default.' },
  { key: 'auth', title: 'Online authentication',
    text: 'Verifies accounts with Mojang and forwards identities to Paper with Velocity modern forwarding.' },
  { key: 'lua', title: 'Configured in Lua',
    text: 'An ordinary script with modules and folder plugins. Check it, then apply with a reload.' },
];

export const MORE_FEATURES = [
  { title: 'Web dashboard', text: 'Edit, validate and apply config from a browser. Prometheus metrics built in.' },
  { title: 'Extensions', text: 'Login and transfer hooks, permission-checked commands, FIFO queues, durable state.' },
  { title: 'Plugin messaging', text: 'A subject broker for Lua, Rust and server plugins over authenticated QUIC.' },
  { title: 'BungeeCord channel', text: 'Existing Bukkit and Paper plugins can transfer players and query the network.' },
  { title: 'Managed servers', text: 'Local processes, service groups, scaling, templates and persistent worlds.' },
];

// The servers in the switching demo and the command that reaches each one.
export const WORLDS = [
  { name: 'lobby', cmd: '/hub' },
  { name: 'survival', cmd: '/server survival' },
  { name: 'games', cmd: '/server games' },
];

// Guides featured on the landing page, with shorter blurbs than the docs nav.
export const GUIDES = [
  { slug: 'overview', blurb: 'Install, first run, auth and routing' },
  { slug: 'lua', blurb: 'Settings, modules and folder plugins' },
  { slug: 'extensions', blurb: 'Hooks, commands, queues and state' },
  { slug: 'operations', blurb: 'systemd, containers, upgrades, rollback' },
  { slug: 'managed-servers', blurb: 'Processes, scaling, templates, worlds' },
  { slug: 'http', blurb: 'Dashboard, status site, config API' },
  { slug: 'messaging', blurb: 'The subject broker and its QUIC protocol' },
  { slug: 'bungeecord', blurb: 'Transfers for Bukkit/Paper plugins' },
  { slug: 'performance', blurb: 'CPU, memory and latency vs. Velocity' },
];

export function fmt(n, digits = 0) {
  return n.toLocaleString('en-US', { minimumFractionDigits: digits, maximumFractionDigits: digits });
}

// Builds a complete HTML document around a page's body markup.
export function shell({ title, description, fonts, styles, scripts = [], body, base, bodyClass = '', themeColor }) {
  const fontLinks = fonts
    ? `<link rel="preconnect" href="https://fonts.googleapis.com"><link rel="preconnect" href="https://fonts.gstatic.com" crossorigin><link rel="stylesheet" href="${fonts}">`
    : '';
  return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>${escapeHtml(title)}</title>
<meta name="description" content="${escapeHtml(description)}">
${themeColor ? `<meta name="theme-color" content="${themeColor}">` : ''}
<link rel="icon" href="${base}favicon.svg" type="image/svg+xml">
${fontLinks}
${styles.map((s) => `<link rel="stylesheet" href="${s}">`).join('\n')}
</head>
<body class="${bodyClass}">
${body}
${scripts.map((s) => `<script src="${s}" defer></script>`).join('\n')}
</body>
</html>
`;
}
