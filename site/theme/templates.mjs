// The documentation as a game GUI: bevelled windows, a hotbar for navigation
// (keys 1 to 9) and chat colours for code.
import { SITE, QUICK_START, PERF, FEATURES, fmt } from '../lib/site.mjs';
import { neighbours, readingMinutes, escapeHtml, pixelSvg } from '../lib/view.mjs';

export const meta = {
  fonts: 'https://fonts.googleapis.com/css2?family=Pixelify+Sans:wght@400;600;700&family=Rubik:ital,wght@0,400;0,500;0,600;1,400&family=Source+Code+Pro:wght@400;600&display=swap',
  themeColor: '#7fb2f0',
};

const P = {
  k: '#2b2b2b', w: '#ececec', r: '#b02e26', y: '#f2c230', l: '#3c5ec6', c: '#4fd6d6',
  s: '#8b8d8f', S: '#6b6d6f', b: '#7a5230', g: '#5b8c32', G: '#7cbd45', d: '#866043',
  D: '#6b4a33', p: '#a34fd1', o: '#ffaa00', t: '#e0ac7e', n: '#3b2a1e', u: '#34469c', e: '#1f6f6a', E: '#2fa89a',
};

const ICONS = {
  book: ['..kkkkk.', '.krrrrwk', '.krrrrwk', '.kryyrwk', '.krrrrwk', '.krrrrwk', '.kkkkkwk', '..kkkkk.'],
  lua: ['..llll..', '.llllww.', 'lllllwwl', 'llllllll', 'llwwllll', 'llwwllll', '.llllll.', '..llll..'],
  key: ['....yyy.', '...y..y.', '...y..y.', '....yyy.', '...y....', '..yy....', '.y.y....', 'yy......'],
  pick: ['.cccccc.', 'c..bb..c', '...bb...', '...bb...', '...bb...', '...bb...', '...bb...', '...bb...'],
  compass: ['..kkkk..', '.kssssk.', 'kssrsssk', 'kssrsssk', 'ksswsssk', 'ksswsssk', '.kssssk.', '..kkkk..'],
  letter: ['........', 'wwwwwwww', 'wkwwwwkw', 'wwkwwkww', 'wwwkkwww', 'wwwwwwww', 'wwwwwwww', '........'],
  chain: ['..ss....', '.s..s...', '.s..s...', '..ssss..', '...s..s.', '...s..s.', '....ss..', '........'],
  torch: ['...rr...', '..royr..', '...rr...', '...bb...', '...bb...', '...bb...', '...bb...', '........'],
  potion: ['...kk...', '...ww...', '..kwwk..', '.kcccck.', '.kcwcck.', '.kcccck.', '..kkkk..', '........'],
  grass: ['gGgggGgg', 'gggGgggg', 'dgdgddgd', 'dddddddd', 'ddDddddd', 'dddddDdd', 'dDdddddd', 'ddddddDd'],
  dirt: ['dddDdddd', 'dDdddddd', 'ddddddDd', 'dddddddd', 'ddDddddd', 'dddddDdd', 'dDdddddd', 'ddddddDd'],
  stone: ['ssssSsss', 'sSssssss', 'ssssssSs', 'ssSsssss', 'ssssssss', 'sssSssss', 'Ssssssss', 'sssssSss'],
  shield: ['.kkkkkk.', 'klllllk.', 'klwwllk.', 'kllwllk.', '.klllk..', '.klllk..', '..klk...', '...k....'],
  gear: ['..s..s..', '.ssssss.', 'sssSSsss', '.sSkkSs.', '.sSkkSs.', 'sssSSsss', '.ssssss.', '..s..s..'],
  map: ['kkkkkkkk', 'kyyyyyyk', 'kygggyyk', 'kyggyyyk', 'kyyylyyk', 'kyyllyyk', 'kyyyyyyk', 'kkkkkkkk'],
  pearl: ['..eeee..', '.eEEEEe.', 'eEcEEEEe', 'eEccEEEe', 'eEEEEEEe', 'eEEEEEEe', '.eEEEEe.', '..eeee..'],
  bell: ['...yy...', '..yyyy..', '..yyyy..', '.yyyyyy.', '.yyyyyy.', 'yyyyyyyy', '...kk...', '........'],
};
const icon = (name, size = 32, label = '') => pixelSvg(ICONS[name], P, { size, label });

// Hotbar slots. Messaging subpages light up the messaging slot.
const HOTBAR = [
  ['overview', 'book'], ['lua', 'lua'], ['extensions', 'key'], ['operations', 'pick'], ['http', 'compass'],
  ['messaging', 'letter'], ['bungeecord', 'chain'], ['network-protocol', 'torch'], ['performance', 'potion'],
];
const SLOT_OF = { 'messaging-lua': 'messaging', 'messaging-protocol': 'messaging' };
const FEATURE_ICONS = ['map', 'shield', 'lua', 'key', 'pearl', 'compass', 'letter', 'bell'];

function hotbar(ctx, current) {
  const active = SLOT_OF[current] ?? current;
  return `<nav class="hotbar" aria-label="Documentation hotbar">
  <ol>
    ${HOTBAR.map(([slug, ic], i) => {
      const d = ctx.docs.find((x) => x.slug === slug);
      return `<li><a href="${ctx.href.doc(slug)}" data-key="${i + 1}"${slug === active ? ' aria-current="page"' : ''}>${icon(ic, 32)}<span class="tip">${d.title}<small>${d.blurb}</small></span><span class="n" aria-hidden="true">${i + 1}</span></a></li>`;
    }).join('')}
  </ol>
</nav>`;
}

function topbar(ctx) {
  return `<header class="topbar">
  <a class="logo" href="${ctx.href.home}">Rift</a>
  <nav aria-label="Site"><a class="mc-btn small" href="${SITE.repo}">GitHub</a><a class="mc-btn small" href="${SITE.releases}">Download</a></nav>
</header>`;
}

function island(name, top, rows, i) {
  const blocks = (n, kind) => Array.from({ length: n }, () => `<span class="blk">${icon(kind, 40)}</span>`).join('');
  return `<div class="island" data-island="${i}">
    <p class="sign">${name}</p>
    <div class="rowb">${blocks(top, 'grass')}</div>
    ${rows.map((n, j) => `<div class="rowb">${blocks(n, j ? 'stone' : 'dirt')}</div>`).join('')}
  </div>`;
}

const PLAYER = pixelSvg([
  '..nnnn..', '..nttn..', '..tttt..', '..tttt..', '.pppppp.', 'tppppppt', 'tppppppt', 't.pppp.t',
  '..uuuu..', '..uuuu..', '..u..u..', '..k..k..',
], P, { size: 32 });

export function home(ctx) {
  const worlds = [['lobby', 3, [2, 1]], ['survival', 4, [3, 2]], ['games', 3, [2, 1]]];
  const body = `${topbar(ctx)}
<main id="main">
  <section class="sky">
    <h1 class="title">Rift</h1>
    <p class="splash" aria-hidden="true">Written in Rust!</p>
    <p class="sub">${SITE.summary}</p>
    <div class="scene" data-scene style="--i:0">
      <div class="player" aria-hidden="true"><div class="hop">${PLAYER}</div></div>
      ${worlds.map(([n, t, r], i) => island(n, t, r, i)).join('')}
    </div>
    <div class="chat" data-chat>
      <ol class="log" aria-live="polite"><li><span class="gray">You joined through play.example.com and landed in</span> <span class="gold">lobby</span></li></ol>
      <div class="cmds" role="group" aria-label="Try a command">
        <button type="button" data-go="1">/server survival</button>
        <button type="button" data-go="2">/server games</button>
        <button type="button" data-go="0">/hub</button>
      </div>
    </div>
  </section>

  <div class="ground" aria-hidden="true"></div>

  <section class="window start">
    <h2>Quick start</h2>
    <p>Download the archive for your platform from <a href="${SITE.releases}">Releases</a>, extract it and run:</p>
    <ol class="recipe">
      ${QUICK_START.map((s) => `<li><code>${escapeHtml(s.cmd)}</code><span>${s.note}</span></li>`).join('')}
    </ol>
    <p class="hint">Java ${SITE.versions.from} to ${SITE.versions.to}. Backends and clients use the same version; Rift never translates.</p>
    <div class="row-btns"><a class="mc-btn" href="${SITE.releases}">Download</a><a class="mc-btn" href="${ctx.href.doc('overview')}">Open the guide</a></div>
  </section>

  <section class="window inv">
    <h2>Inventory</h2>
    <p class="hint">Hover or focus a slot to read what it does.</p>
    <ul class="slots">
      ${FEATURES.map((f, i) => `<li><button type="button" class="slot" aria-describedby="f${i}">${icon(FEATURE_ICONS[i], 36)}<span class="tip" id="f${i}" role="tooltip">${f.title}<small>${f.text}</small></span><span class="sr">${f.title}</span></button></li>`).join('')}
    </ul>
    <dl class="plain">
      ${FEATURES.map((f) => `<div><dt>${f.title}</dt><dd>${f.text}</dd></div>`).join('')}
    </dl>
  </section>

  <section class="window stats">
    <h2>Stats</h2>
    <p class="hint">Median of three loopback trials with ${PERF.clients} clients. Shorter bar is better.</p>
    ${[PERF.memory, PERF.loginCpu].map((m) => {
      const pct = (v) => (v / Math.max(m.rift, m.velocity)) * 100;
      return `<div class="stat"><h3>${m.label}</h3>
        <div class="xp"><span style="width:${Math.max(pct(m.rift), 2).toFixed(1)}%"></span></div><p><b>Rift</b> ${fmt(m.rift, 2)} ${m.unit}</p>
        <div class="xp velo"><span style="width:${pct(m.velocity).toFixed(1)}%"></span></div><p><b>Velocity</b> ${fmt(m.velocity, 2)} ${m.unit}</p></div>`;
    }).join('')}
    <p class="hint"><a href="${ctx.href.doc('performance')}">Method and limitations</a></p>
  </section>

  <section class="window books">
    <h2>Guides</h2>
    <p class="hint">Press a number key to jump to a hotbar slot.</p>
    <ul>${ctx.docs.map((d) => `<li><a href="${ctx.href.doc(d.slug)}"><b>${d.title}</b><span>${d.blurb}</span></a></li>`).join('')}</ul>
  </section>
  <p class="legal">Rift is ${SITE.license} licensed and not affiliated with Mojang or Microsoft.</p>
</main>
${hotbar(ctx, '')}`;
  return { title: 'Rift', description: SITE.description, body, bodyClass: 'home' };
}

export function doc(ctx, d) {
  const { prev, next } = neighbours(ctx.docs, d);
  const group = SLOT_OF[d.slug] ?? d.slug;
  const tabs = group === 'messaging'
    ? `<nav class="tabs" aria-label="Messaging pages">${['messaging', 'messaging-lua', 'messaging-protocol'].map((s) => `<a href="${ctx.href.doc(s)}"${s === d.slug ? ' aria-current="page"' : ''}>${ctx.docs.find((x) => x.slug === s).title}</a>`).join('')}</nav>`
    : '';
  const toc = d.toc.filter((t) => t.depth === 2);
  const body = `${topbar(ctx)}
<main id="main" class="dirt">
  <article class="window page">
    ${tabs}
    <p class="hint">${d.group}, ${readingMinutes(d)} minute read</p>
    <h1>${escapeHtml(d.heading)}</h1>
    ${toc.length > 1 ? `<details class="contents"><summary>Contents</summary><ul>${toc.map((t) => `<li><a href="#${t.id}">${t.text}</a></li>`).join('')}</ul></details>` : ''}
    <div class="prose">${d.html}</div>
    <nav class="row-btns pager" aria-label="Previous and next">
      ${prev ? `<a class="mc-btn" href="${ctx.href.doc(prev.slug)}">Back: ${prev.title}</a>` : ''}
      ${next ? `<a class="mc-btn" href="${ctx.href.doc(next.slug)}">Next: ${next.title}</a>` : ''}
    </nav>
  </article>
</main>
${hotbar(ctx, d.slug)}`;
  return { title: `${d.title} – Rift`, description: d.blurb, body, bodyClass: 'docpage' };
}

export function notFound(ctx) {
  const body = `${topbar(ctx)}
<main id="main" class="dirt">
  <article class="window page">
    <h1>Page not found</h1>
    <p>This chunk has not been generated. The page may have moved.</p>
    <div class="row-btns"><a class="mc-btn" href="${ctx.href.home}">Back to spawn</a><a class="mc-btn" href="${ctx.href.doc('overview')}">Open the guide</a></div>
  </article>
</main>
${hotbar(ctx, '')}`;
  return { title: 'Page not found – Rift', description: SITE.description, body, bodyClass: 'docpage' };
}
