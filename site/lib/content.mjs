// Turns the repository's Markdown (docs/*.md and README.md) into page data.
// The Markdown stays the single source of truth; release archives ship it too.
import { readFile } from 'node:fs/promises';
import { Marked } from 'marked';
import { createHighlighter, createCssVariablesTheme } from 'shiki';

export const REPO = 'https://github.com/Jotrorox/rift';
const BLOB = `${REPO}/blob/master/`;

// Navigation order and grouping. `file` is relative to the repository root.
export const DOCS = [
  { slug: 'overview', file: 'README.md', title: 'Overview', group: 'Start',
    blurb: 'Install, first run, authentication and hostname routing.' },
  { slug: 'lua', file: 'docs/lua.md', title: 'Lua configuration', group: 'Configure',
    blurb: 'Script-style settings, local modules and folder plugins.' },
  { slug: 'extensions', file: 'docs/extensions.md', title: 'Authenticated extensions', group: 'Configure',
    blurb: 'Login and transfer hooks, commands, permissions, queues and durable state.' },
  { slug: 'operations', file: 'docs/operations.md', title: 'Operations', group: 'Run',
    blurb: 'systemd, containers, reloads, maintenance, upgrades and rollback.' },
  { slug: 'managed-servers', file: 'docs/managed-servers.md', title: 'Managed servers', group: 'Run',
    blurb: 'Local processes, service groups, scaling, templates and persistent worlds.' },
  { slug: 'http', file: 'docs/http.md', title: 'Web dashboard and HTTP', group: 'Run',
    blurb: 'The bundled dashboard, status site, config API and Lua endpoints.' },
  { slug: 'messaging', file: 'docs/messaging.md', title: 'Messaging', group: 'Integrate',
    blurb: 'A subject broker shared by Rust, Lua and QUIC server plugins.' },
  { slug: 'messaging-lua', file: 'docs/messaging-lua.md', title: 'Messaging from Lua', group: 'Integrate',
    blurb: 'Subscriptions, publishing and limits inside Lua callbacks.' },
  { slug: 'messaging-protocol', file: 'docs/messaging-protocol.md', title: 'Messaging protocol', group: 'Integrate',
    blurb: 'The QUIC wire format for clients in any language.' },
  { slug: 'bungeecord', file: 'docs/bungeecord.md', title: 'BungeeCord compatibility', group: 'Integrate',
    blurb: 'Transfers and player queries for existing Bukkit/Paper plugins.' },
  { slug: 'network-protocol', file: 'docs/network-protocol.md', title: 'Network protocol', group: 'Reference',
    blurb: 'Backend switching and recovery for Java 1.8.9 through 26.3.' },
  { slug: 'performance', file: 'docs/performance.md', title: 'Performance', group: 'Reference',
    blurb: 'CPU, memory and latency against Velocity, with reproducible runs.' },
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
function rewriteHref(href, from, docHref) {
  if (/^[a-z]+:/i.test(href) || href.startsWith('#')) return href;
  const [path, hash = ''] = href.split('#');
  const dir = from.includes('/') ? from.slice(0, from.lastIndexOf('/') + 1) : '';
  const parts = [];
  for (const part of (dir + path).split('/')) {
    if (part === '..') parts.pop();
    else if (part && part !== '.') parts.push(part);
  }
  const target = parts.join('/');
  const doc = bySource.get(target);
  if (doc) return docHref(doc.slug) + (hash ? `#${hash}` : '');
  return BLOB + target + (hash ? `#${hash}` : '');
}

const LANGS = ['sh', 'lua', 'yaml', 'properties', 'rust', 'python', 'http', 'java', 'json', 'text'];
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
        if (depth <= 3) toc.push({ depth, id, text: inner.replace(/<[^>]+>/g, '') });
        return `<h${depth} id="${id}"><a class="anchor" href="#${id}" aria-hidden="true" tabindex="-1">#</a>${inner}</h${depth}>\n`;
      },
      code({ text, lang }) {
        return highlight(hl, text, lang);
      },
      link({ href, title: linkTitle, tokens }) {
        const inner = this.parser.parseInline(tokens);
        const url = rewriteHref(href, doc.file, docHref);
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
        if (!lead) lead = html.replace(/<[^>]+>/g, '');
        return `<p>${html}</p>\n`;
      },
    },
  });

  const html = marked.parse(source);
  const words = source.split(/\s+/).filter(Boolean).length;
  return { ...doc, heading, html, toc, lead, words, lines: source.split('\n').length };
}

export async function loadDocs(root, docHref) {
  return Promise.all(DOCS.map((d) => renderDoc(root, d, docHref)));
}
