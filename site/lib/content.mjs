// Turns the repository's Markdown (docs/*.md and README.md) into page data.
// The Markdown stays the single source of truth; release archives ship it too.
import { access, readFile, readdir } from 'node:fs/promises';
import { Marked } from 'marked';
import { createHighlighter, createCssVariablesTheme } from 'shiki';

export const REPO = 'https://github.com/Jotrorox/rift';
const BLOB = `${REPO}/blob/master/`;

// Navigation order and grouping. `file` is relative to the repository root.
// Every Markdown file in docs/ must be listed; the build fails otherwise.
const page = (group, slug, title, blurb, file = `docs/${slug}.md`) => ({ slug, file, title, group, blurb });

export const DOCS = [
  page('Start', 'overview', 'Overview', 'What Rift is, what it can do and a quick start.', 'README.md'),
  page('Start', 'install', 'Install', 'Download, verify, first start and configuration checks.'),
  page('Start', 'authentication', 'Authentication', 'Online mode, Velocity and BungeeGuard forwarding, and signed chat.'),

  page('Configure', 'lua', 'Lua configuration', 'Settings as a Lua script, local modules and folder plugins.'),
  page('Configure', 'server-list', 'Server list and tab list', 'MOTD, favicon and player count; tab list header, footer and network-wide players.'),
  page('Configure', 'text', 'Styled text', 'Markup, legacy codes, themes and translating Rift\'s own messages.'),
  page('Configure', 'translation', 'Protocol translation', 'Players and backends on different Minecraft versions.'),
  page('Configure', 'bungeecord', 'BungeeCord compatibility', 'Transfers, queries, forwarding, messages and kicks for existing Bukkit/Paper plugins.'),

  page('Operate', 'operations', 'Day-to-day operations', 'Reloads, maintenance mode, draining and transfers.'),
  page('Operate', 'admin', 'Admin CLI', 'The admin endpoint, its token and permissions, and every rift admin command.'),
  page('Operate', 'deployment', 'Deployment', 'Run Rift under systemd or in a container.'),
  page('Operate', 'database', 'SQLite or PostgreSQL', 'Where Rift keeps its state, using PostgreSQL, standby controllers and copying between backends.'),
  page('Operate', 'monitoring', 'Monitoring and logs', 'Prometheus metrics, connection events, log levels and what to do about each error.'),
  page('Operate', 'hardening', 'Hardening', 'Rate limits, PROXY protocol behind a load balancer, and how Rift guards its secrets.'),
  page('Operate', 'upgrading', 'Upgrade and rollback', 'Graceful shutdown, upgrades, rollback and the forward-only database.'),

  page('Servers', 'managed-servers', 'Managed servers', 'Minecraft server processes that Rift starts on demand and stops when empty.'),
  page('Servers', 'service-groups', 'Service groups and scaling', 'Instances created while Rift runs, with allocated ports and automatic scaling.'),
  page('Servers', 'templates', 'Templates and software', 'Jars, plugins, configs and maps for new instances; Paper, Folia, Purpur, Fabric or vanilla downloads.'),
  page('Servers', 'kept-servers', 'Servers that outlive the proxy', 'Keep servers running through a Rift restart, upgrade or crash.'),
  page('Servers', 'server-limits', 'Isolation and memory limits', 'Confine server processes to their directory and cap their memory on Linux.'),
  page('Servers', 'bus-logins', 'Bus logins for servers', 'A message bus login for every managed server and group instance.'),
  page('Servers', 'nodes', 'Multiple hosts', 'Game servers on more machines with rift node agents, placement and failure handling.'),

  page('Web', 'http', 'Web dashboard', 'The bundled dashboard, its tokens and operators, and browser-based configuration.'),
  page('Web', 'status-page', 'Status page', 'A public, read-only status site for your network.'),
  page('Web', 'http-api', 'HTTP API', 'Configuration, servers, groups, templates, files and audit records over HTTP.'),
  page('Web', 'http-lua', 'Lua endpoints', 'Websites and endpoints served from Lua.'),

  page('Plugins', 'plugins', 'Plugins', 'One contract, three ways to run code, plugin.toml and the trust model.'),
  page('Plugins', 'plugin-events', 'Events and commands', 'Every event and its answers, chat, background work and commands.'),
  page('Plugins', 'plugin-operations', 'Running plugins', 'Lifecycle, failures, restarts, reloads and what operators can see.'),
  page('Plugins', 'extensions', 'Hooks in rift.lua', 'Login and transfer decisions, commands and tab completion in the configuration script.'),
  page('Plugins', 'remote-decisions', 'Remote decisions', 'Let a service on the message bus answer login, transfer and kick decisions.'),
  page('Plugins', 'queues', 'Server queues', 'Backend capacity, waiting lines, rank priority and bypass.'),
  page('Plugins', 'host-services', 'Host services', 'Durable state, timers, HTTP integrations and permissions for plugin code.'),
  page('Plugins', 'players-api', 'Online players API', 'Messages, titles, tab lists, kicks, broadcasts and transfers.'),
  page('Plugins', 'dialogs', 'Dialogs', 'Screens with inputs and buttons on 1.21.6+, shown by plugins through the proxy.'),
  page('Plugins', 'plugins-lua', 'Lua plugins', 'Persistent plugins in their own LuaJIT state, and the JIT switch.'),
  page('Plugins', 'plugins-rust', 'Rust plugins', 'WebAssembly components with the rift-plugin crate.'),
  page('Plugins', 'plugins-c', 'C plugins', 'WebAssembly components with wasi-sdk and the C SDK.'),
  page('Plugins', 'plugins-go', 'Go plugins', 'WebAssembly components with TinyGo and the Go SDK.'),

  page('Messaging', 'messaging', 'Messaging', 'A subject broker shared by Rust, Lua and QUIC server plugins.'),
  page('Messaging', 'messaging-subjects', 'Built-in subjects', 'The player, network, control and server-state subjects Rift answers.'),
  page('Messaging', 'messaging-lua', 'Messaging from Lua', 'Subscriptions, publishing and limits inside Lua callbacks.'),
  page('Messaging', 'network-java', 'Network API for Java', 'List servers, find and connect players, and message proxy plugins from Paper.'),
  page('Messaging', 'messaging-protocol', 'Messaging protocol', 'The QUIC wire format for clients in any language.'),

  page('Permissions', 'permissions', 'Permissions', 'One permission store for the network: groups, tracks, contexts and visibility.'),
  page('Permissions', 'permissions-commands', 'Permission commands', 'The /perms grammar, rift admin perms, doctor and LuckPerms import.'),
  page('Permissions', 'permissions-lua', 'Permissions from Lua', 'rift.permissions, change events, context providers and persistent plugins.'),
  page('Permissions', 'permissions-http', 'Permissions HTTP API', 'Read and edit the permission store over /api/perms.'),
  page('Permissions', 'riftperms', 'RiftPerms for Paper', 'The backend plugin for Paper and Folia: install, settings, Vault and placeholders.'),
  page('Permissions', 'permissions-java', 'Permissions API for Java', 'Read, write and listen for permission changes from a Java plugin.'),
  page('Permissions', 'permissions-pumpkin', 'Permissions for Pumpkin', 'The Pumpkin adapter plugin, its configuration and service API.'),
  page('Permissions', 'permissions-clients', 'Rust and C clients', 'The Rust client, the in-memory testing crate and the C ABI.'),
  page('Permissions', 'permissions-protocol', 'Permissions protocol', 'Bus subjects, JSON shapes and the check algorithm every replica runs.'),

  page('Reference', 'network-protocol', 'Network protocol', 'Supported releases, login, forwarding and backend switching for Java 1.8.9 through 26.3.'),
  page('Reference', 'translation-internals', 'How translation works', 'The translation pipeline, its data sources and how it is verified.'),
  page('Reference', 'stability', 'Stability and versioning', 'Release channels, what a version number promises and how the database upgrades.'),
  page('Reference', 'performance', 'Performance', 'CPU, memory and latency against Velocity, with reproducible runs.'),
];

export const GROUPS = [...new Set(DOCS.map((d) => d.group))];

// README sections that the website's own navigation replaces.
const README_SKIP = new Set(['Documentation']);

export function slugify(text) {
  return text.toLowerCase().trim()
    .replace(/<[^>]+>/g, '')
    .replace(/[^\p{L}\p{N}\s_-]/gu, '')
    .replace(/\s/g, '-');
}

export function escapeHtml(text) {
  return String(text).replace(/[&<>"']/g, (c) =>
    ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);
}

const bySource = new Map(DOCS.map((d) => [d.file, d]));

// Maps a Markdown link written relative to `from` onto the site or GitHub.
function rewriteHref(href, from, docHref, seen) {
  if (/^[a-z]+:/i.test(href)) return href;
  if (href.startsWith('#')) { seen.push({ href, slug: bySource.get(from).slug, hash: href.slice(1) }); return href; }
  const [path, hash = ''] = href.split('#');
  const dir = from.includes('/') ? from.slice(0, from.lastIndexOf('/') + 1) : '';
  const parts = [];
  for (const part of (dir + path).split('/')) {
    if (part === '..') parts.pop();
    else if (part && part !== '.') parts.push(part);
  }
  const target = parts.join('/');
  const doc = bySource.get(target);
  if (doc) { seen.push({ href, slug: doc.slug, hash }); return docHref(doc.slug) + (hash ? `#${hash}` : ''); }
  seen.push({ href, path: target });
  return BLOB + target + (hash ? `#${hash}` : '');
}

const LANGS = ['sh', 'lua', 'yaml', 'properties', 'rust', 'python', 'http', 'java', 'kotlin', 'groovy', 'xml', 'json', 'toml', 'c', 'go', 'text'];
let highlighter;

function getHighlighter() {
  highlighter ??= createHighlighter({
    themes: [createCssVariablesTheme({ name: 'css-variables', variablePrefix: '--code-', fontStyle: true })],
    langs: LANGS.filter((l) => l !== 'text'),
  });
  return highlighter;
}

function highlight(hl, code, rawLang) {
  let lang = (rawLang || 'text').split(',')[0].trim();
  if (lang === 'rust') {
    // Hide rustdoc's `# ` setup lines, as rustdoc does when rendering.
    code = code.split('\n').filter((l) => !/^#( |$)/.test(l)).join('\n');
  }
  if (!LANGS.includes(lang)) lang = 'text';
  const html = hl.codeToHtml(code, { lang, theme: 'css-variables' });
  return `<figure class="code" data-lang="${lang}">${html}</figure>`;
}

function stripReadme(markdown) {
  const out = [];
  let skipping = false;
  for (const line of markdown.split('\n')) {
    const h2 = line.match(/^## (.+)/);
    if (h2) skipping = README_SKIP.has(h2[1].trim());
    if (!skipping) out.push(line);
  }
  return out.join('\n');
}

// Renders one document. `docHref(slug)` builds the link to another document.
export async function renderDoc(root, doc, docHref) {
  const hl = await getHighlighter();
  let source = await readFile(new URL(doc.file, root), 'utf8');
  if (doc.file === 'README.md') source = stripReadme(source);

  const toc = [];
  const ids = new Set();
  const links = [];
  const used = new Map();
  let heading = doc.title;
  let lead = '';

  const marked = new Marked({ gfm: true });
  marked.use({
    renderer: {
      heading({ tokens, depth, text }) {
        const inner = this.parser.parseInline(tokens);
        if (depth === 1) { if (doc.slug !== 'overview') heading = text; return ''; }
        let id = slugify(text);
        const n = used.get(id) ?? 0;
        used.set(id, n + 1);
        if (n) id = `${id}-${n}`;
        ids.add(id);
        if (depth <= 3) toc.push({ depth, id, text: inner.replace(/<[^>]+>/g, '') });
        return `<h${depth} id="${id}"><a class="anchor" href="#${id}" aria-hidden="true" tabindex="-1">#</a>${inner}</h${depth}>\n`;
      },
      code({ text, lang }) {
        return highlight(hl, text, lang);
      },
      link({ href, title: linkTitle, tokens }) {
        const inner = this.parser.parseInline(tokens);
        const url = rewriteHref(href, doc.file, docHref, links);
        const external = /^https?:/.test(url) ? ' rel="noopener"' : '';
        const t = linkTitle ? ` title="${escapeHtml(linkTitle)}"` : '';
        return `<a href="${escapeHtml(url)}"${t}${external}>${inner}</a>`;
      },
      table(token) {
        // Wrap tables so wide ones scroll inside the column, not the page.
        return `<div class="table-wrap">${this.constructor.prototype.table.call(this, token)}</div>`;
      },
      paragraph({ tokens }) {
        const html = this.parser.parseInline(tokens);
        if (!lead && !html.startsWith('[!')) lead = html.replace(/<[^>]+>/g, '');
        return `<p>${html}</p>\n`;
      },
      blockquote({ tokens }) {
        // GitHub alerts: a quote that opens with [!NOTE], [!TIP] or [!WARNING].
        const html = this.parser.parse(tokens);
        const alert = html.match(/^<p>\[!(NOTE|TIP|WARNING)\]\s*/);
        if (!alert) return `<blockquote>${html}</blockquote>\n`;
        const kind = alert[1].toLowerCase();
        const label = kind[0].toUpperCase() + kind.slice(1);
        return `<aside class="callout callout-${kind}"><p><strong class="callout-label">${label}</strong> ${html.slice(alert[0].length)}</aside>\n`;
      },
    },
  });

  const html = marked.parse(source);
  const words = source.split(/\s+/).filter(Boolean).length;
  return { ...doc, heading, html, toc, ids, links, lead, words, lines: source.split('\n').length };
}

// Every relative link must lead to a listed page (and an existing heading on
// it) or to a file in the repository, and every page in docs/ must be listed.
async function problems(root, docs) {
  const found = [];
  const bySlug = new Map(docs.map((d) => [d.slug, d]));
  for (const doc of docs) {
    for (const link of doc.links) {
      if (link.slug) {
        if (link.hash && !bySlug.get(link.slug).ids.has(link.hash)) found.push(`${doc.file}: no heading for ${link.href}`);
      } else if (!(await access(new URL(link.path, root)).then(() => true, () => false))) {
        found.push(`${doc.file}: no file for ${link.href}`);
      }
    }
  }
  for (const name of await readdir(new URL('docs/', root))) {
    if (name.endsWith('.md') && !bySource.has(`docs/${name}`)) found.push(`docs/${name}: not listed in DOCS (site/lib/content.mjs)`);
  }
  return found;
}

export async function loadDocs(root, docHref) {
  const docs = await Promise.all(DOCS.map((d) => renderDoc(root, d, docHref)));
  const found = await problems(root, docs);
  if (found.length) throw new Error(`Documentation links are broken:\n  ${found.join('\n  ')}`);
  return docs;
}
