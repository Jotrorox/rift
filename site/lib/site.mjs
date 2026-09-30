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
  { cmd: './rift init rift.lua', note: 'Write a commented starter configuration' },
  { cmd: './rift check rift.lua', note: 'Validate it without opening any ports' },
  { cmd: './rift --config rift.lua', note: 'Start the proxy' },
];

// Median values, 16 concurrent clients, from docs/performance.md.
export const PERF = {
  clients: 16,
  memory: { rift: 8.86, velocity: 354.40, unit: 'MiB', label: 'Peak memory' },
  loginCpu: { rift: 54.0, velocity: 208.12, unit: '%', label: 'CPU during login bursts' },
};

export const FEATURES = [
  { key: 'routing', title: 'Hostname routing',
    text: 'One listener serves many servers. Exact names win, then the longest wildcard, then the default.' },
  { key: 'auth', title: 'Online authentication',
    text: 'Rift verifies accounts with Mojang and forwards identities to Paper with Velocity modern forwarding.' },
  { key: 'lua', title: 'Configured in Lua',
    text: 'An ordinary script with modules and folder plugins. Validate with rift check, apply with a reload.' },
  { key: 'extensions', title: 'Extensions',
    text: 'Login and transfer hooks, permission-checked commands, FIFO queues and durable state.' },
  { key: 'switching', title: 'Server switching',
    text: '/server, /hub and crash recovery keep the client connected while the backend changes.' },
  { key: 'dashboard', title: 'Web dashboard',
    text: 'Edit, validate and apply the configuration from a browser. Status and Prometheus metrics built in.' },
  { key: 'messaging', title: 'Plugin messaging',
    text: 'A subject broker shared by Lua, Rust and server plugins over authenticated QUIC.' },
  { key: 'bungeecord', title: 'BungeeCord channel',
    text: 'Existing Bukkit and Paper plugins can transfer players and query the network.' },
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
