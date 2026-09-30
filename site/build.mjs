// Builds the static documentation site into dist/.
//
//   node build.mjs            build with base "/" (or $SITE_BASE, e.g. "/rift/")
//   node build.mjs --serve    build, then serve dist/ on http://localhost:4321
import { cp, mkdir, rm, writeFile, readFile, stat } from 'node:fs/promises';
import { createServer } from 'node:http';
import { extname, join } from 'node:path';
import { loadDocs } from './lib/content.mjs';
import { shell } from './lib/site.mjs';
import * as theme from './theme/templates.mjs';

const here = new URL('./', import.meta.url);
const root = new URL('../', here);
const dist = new URL('dist/', here);
const base = normalizeBase(process.env.SITE_BASE ?? '/');

function normalizeBase(value) {
  return `/${value.replace(/^\/+|\/+$/g, '')}/`.replace(/^\/\/$/, '/');
}

async function write(path, content) {
  const url = new URL(path, dist);
  await mkdir(new URL('./', url), { recursive: true });
  await writeFile(url, content);
}

async function build() {
  await rm(dist, { recursive: true, force: true });
  await mkdir(dist, { recursive: true });
  await cp(new URL('public/', here), dist, { recursive: true });
  await cp(new URL('theme/assets/', here), new URL('assets/', dist), { recursive: true });

  const href = {
    home: base,
    doc: (slug) => `${base}docs/${slug}/`,
  };
  const docs = await loadDocs(root, href.doc);
  const ctx = { base, href, docs };

  const page = (view) => shell({
    ...view,
    base,
    fonts: theme.meta.fonts,
    themeColor: theme.meta.themeColor,
    styles: [`${base}assets/style.css`, ...(view.styles ?? [])],
    scripts: [`${base}assets/app.js`, ...(view.scripts ?? [])],
  });

  await write('index.html', page(theme.home(ctx)));
  for (const doc of docs) {
    await write(`docs/${doc.slug}/index.html`, page(theme.doc(ctx, doc)));
  }
  if (theme.notFound) await write('404.html', page(theme.notFound(ctx)));
  console.log(`Built ${docs.length} documents into ${dist.pathname} with base ${base}`);
}

const TYPES = { '.html': 'text/html; charset=utf-8', '.css': 'text/css', '.js': 'text/javascript', '.svg': 'image/svg+xml', '.png': 'image/png', '.txt': 'text/plain' };

function serve(port = Number(process.env.PORT ?? 4321)) {
  createServer(async (req, res) => {
    let path = decodeURIComponent(new URL(req.url, 'http://x').pathname);
    if (!path.startsWith(base)) path = base;
    let file = join(dist.pathname, path.slice(base.length));
    try {
      if ((await stat(file)).isDirectory()) file = join(file, 'index.html');
      res.writeHead(200, { 'content-type': TYPES[extname(file)] ?? 'application/octet-stream' });
      res.end(await readFile(file));
    } catch {
      res.writeHead(404, { 'content-type': TYPES['.html'] });
      res.end(await readFile(join(dist.pathname, '404.html')).catch(() => 'Not found'));
    }
  }).listen(port, () => console.log(`Serving http://localhost:${port}${base}`));
}

await build();
if (process.argv.includes('--serve')) serve();
