// A quiet, product-page theme: one idea per section, system type, translucent
// chrome and spring-driven controls (see assets/app.js).
import { SITE, QUICK_START, PERF, FEATURES, MORE_FEATURES, WORLDS, GUIDES, fmt } from '../lib/site.mjs';
import { GROUPS } from '../lib/content.mjs';
import { neighbours, readingMinutes, escapeHtml } from '../lib/view.mjs';

export const meta = {
  fonts: '',
  themeColor: '#ffffff',
};

const ICONS = {
  routing: '<path d="M4 12h15M13 6l6 6-6 6"/>',
  auth: '<path d="M12 3l7 3v5c0 4.5-3 8.3-7 10-4-1.7-7-5.5-7-10V6l7-3z"/>',
  lua: '<path d="M8 7l-5 5 5 5M16 7l5 5-5 5"/>',
};
const icon = (name) =>
  `<svg class="icon" viewBox="0 0 24 24" width="28" height="28" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round">${ICONS[name]}</svg>`;
const chevron = '<span class="chev" aria-hidden="true">›</span>';

function header(ctx, current = '') {
  const link = (href, label, key) =>
    `<a href="${href}"${key === current ? ' aria-current="page"' : ''}>${label}</a>`;
  return `<a class="skip" href="#main">Skip to content</a>
<header class="nav" data-nav>
  <div class="nav-in">
    <a class="logo" href="${ctx.href.home}">${SITE.name}</a>
    <nav aria-label="Site">
      ${link(ctx.href.doc('overview'), 'Overview', 'overview')}
      ${link(ctx.href.doc('performance'), 'Performance', 'performance')}
      ${link(`${ctx.href.home}#guides`, 'Guides', 'guides')}
      <a href="${SITE.repo}" class="hide-sm">GitHub</a>
      <a class="btn btn-sm" href="${SITE.releases}">Download</a>
    </nav>
  </div>
</header>`;
}

function footer() {
  return `<footer class="foot">
  <div class="wrap foot-in">
    <p>${SITE.name} is ${SITE.license} licensed.</p>
    <p>Not affiliated with Mojang or Microsoft.</p>
  </div>
</footer>`;
}

function demo() {
  const [first] = WORLDS;
  return `<section class="panel demo" aria-labelledby="demo-title">
    <h2 id="demo-title">Switch servers. Stay connected.</h2>
    <p class="lede">Try it — pick a backend. The player never sees a loading screen.</p>
    <div class="seg" role="radiogroup" aria-label="Backend server" data-seg>
      <span class="seg-thumb" aria-hidden="true"><span></span></span>
      ${WORLDS.map((w, i) => `<button type="button" role="radio" aria-checked="${i === 0}" tabindex="${i === 0 ? 0 : -1}" data-cmd="${escapeHtml(w.cmd)}">${w.name}</button>`).join('')}
    </div>
    <div class="term" aria-live="polite" data-term>
      <p class="dim">You joined through play.example.com</p>
      <p class="dim" data-line="cmd">› ${first.cmd}</p>
      <p data-line="out">Connected to <span data-world>${first.name}</span> — same session, no disconnect</p>
    </div>
    <p class="note">/server, /hub and crash recovery keep the client connected while the backend changes.</p>
  </section>`;
}

function performance(ctx) {
  const ratio = Math.round(PERF.memory.velocity / PERF.memory.rift);
  const card = (m) => {
    const max = Math.max(m.rift, m.velocity);
    const row = (name, v, cls) => `<div class="bar-row ${cls}">
          <div class="bar-label"><span>${name}</span><span>${fmt(v, 2)} ${m.unit}</span></div>
          <div class="bar"><span style="--w:${(v / max).toFixed(4)}"></span></div>
        </div>`;
    return `<div class="metric"><h3>${m.label}</h3>${row('Rift', m.rift, 'is-rift')}${row('Velocity', m.velocity, '')}</div>`;
  };
  return `<section class="perf" aria-labelledby="perf-title" data-reveal>
    <p class="eyebrow-muted">${PERF.memory.label} vs. Velocity</p>
    <h2 id="perf-title"><span class="big">${ratio}×</span><span class="big-sub">less memory.</span></h2>
    <div class="metrics">${card(PERF.memory)}${card(PERF.loginCpu)}</div>
    <p class="note">Median of three loopback trials with ${PERF.clients} clients. <a href="${ctx.href.doc('performance')}">Method and limitations ›</a></p>
  </section>`;
}

function features() {
  return `<section class="wrap section" aria-labelledby="features-title">
    <h2 id="features-title" class="h2">Everything your network<br class="hide-sm"> needs. Nothing it doesn't.</h2>
    <ul class="cards">
      ${FEATURES.map((f) => `<li class="card">${icon(f.key)}<h3>${f.title}</h3><p>${f.text}</p></li>`).join('')}
    </ul>
    <dl class="list">
      ${MORE_FEATURES.map((f) => `<div><dt>${f.title}</dt><dd>${f.text}</dd></div>`).join('')}
    </dl>
  </section>`;
}

function quickStart() {
  const { from, to } = SITE.versions;
  return `<section class="wrap section" aria-labelledby="start-title">
    <h2 id="start-title" class="h2">Up and running in three steps.</h2>
    <p class="lede left">No Java. No separate Lua install. Java ${from} to ${to} — clients and backends use the same version.</p>
    <ol class="steps">
      ${QUICK_START.map((s, i) => `<li><span class="step-n">Step ${i + 1}</span><code>${escapeHtml(s.cmd)}</code><p>${s.note}</p></li>`).join('')}
    </ol>
  </section>`;
}

function guides(ctx) {
  return `<section class="wrap section" id="guides" aria-labelledby="guides-title">
    <h2 id="guides-title" class="h3">Guides</h2>
    <ul class="guides">
      ${GUIDES.map((g) => {
        const d = ctx.docs.find((x) => x.slug === g.slug);
        return `<li><a href="${ctx.href.doc(g.slug)}"><span><b>${d.title}</b><small>${g.blurb}</small></span>${chevron}</a></li>`;
      }).join('')}
    </ul>
  </section>`;
}

export function home(ctx) {
  const body = `${header(ctx)}
<main id="main">
  <section class="hero">
    <p class="eyebrow">Minecraft Java Edition proxy</p>
    <h1>One port.<br>Every server.</h1>
    <p class="hero-sub">Route hostnames, authenticate players and switch backends —<br class="hide-sm"> in a single ${fmt(PERF.memory.rift, 2)} ${PERF.memory.unit} process. Written in Rust, scripted in Lua.</p>
    <div class="ctas">
      <a class="btn" href="${SITE.releases}">Download for macOS, Linux &amp; Windows</a>
      <a class="more" href="${ctx.href.doc('overview')}">Read the guide ›</a>
    </div>
  </section>
  <div class="wrap">${demo()}</div>
  ${performance(ctx)}
  ${features()}
  ${quickStart()}
  ${guides(ctx)}
</main>
${footer()}`;
  return { title: `${SITE.name} – One port. Every server.`, description: SITE.description, body, bodyClass: 'home' };
}

function docNav(ctx, current) {
  return GROUPS.map((group) => `<div class="side-group">
      <p>${group}</p>
      <ul>${ctx.docs.filter((d) => d.group === group).map((d) => `<li><a href="${ctx.href.doc(d.slug)}"${d.slug === current ? ' aria-current="page"' : ''}>${d.title}</a></li>`).join('')}</ul>
    </div>`).join('');
}

export function doc(ctx, d) {
  const { prev, next } = neighbours(ctx.docs, d);
  const toc = d.toc.filter((t) => t.depth === 2);
  const tocList = `<ul>${toc.map((t) => `<li><a href="#${t.id}">${escapeHtml(t.text)}</a></li>`).join('')}</ul>`;
  const current = ['overview', 'performance'].includes(d.slug) ? d.slug : '';
  const body = `${header(ctx, current)}
<div class="doc-layout">
  <aside class="side" aria-label="Documentation">
    <details class="side-menu" data-side>
      <summary><span>${d.group}</span> ${escapeHtml(d.title)}</summary>
      <nav>${docNav(ctx, d.slug)}</nav>
    </details>
  </aside>
  <main id="main" class="article">
    <p class="eyebrow">${d.group} · ${readingMinutes(d)} min read</p>
    <h1>${escapeHtml(d.heading)}</h1>
    ${toc.length > 1 ? `<details class="contents"><summary>On this page</summary>${tocList}</details>` : ''}
    <div class="prose">${d.html}</div>
    <nav class="pager" aria-label="Previous and next">
      ${prev ? `<a class="prev" href="${ctx.href.doc(prev.slug)}"><small>Previous</small>${prev.title}</a>` : '<span></span>'}
      ${next ? `<a class="next" href="${ctx.href.doc(next.slug)}"><small>Next</small>${next.title}</a>` : ''}
    </nav>
  </main>
  ${toc.length > 1 ? `<aside class="toc" aria-label="On this page"><p>On this page</p>${tocList}</aside>` : '<span></span>'}
</div>
${footer()}`;
  return { title: `${d.title} – ${SITE.name}`, description: d.blurb, body, bodyClass: 'docpage' };
}

export function notFound(ctx) {
  const body = `${header(ctx)}
<main id="main">
  <section class="hero">
    <p class="eyebrow">404</p>
    <h1>Page not found.</h1>
    <p class="hero-sub">This page may have moved. The guides are a good place to start.</p>
    <div class="ctas">
      <a class="btn" href="${ctx.href.doc('overview')}">Read the guide</a>
      <a class="more" href="${ctx.href.home}">Back to home ›</a>
    </div>
  </section>
</main>
${footer()}`;
  return { title: `Page not found – ${SITE.name}`, description: SITE.description, body, bodyClass: 'notfound' };
}
