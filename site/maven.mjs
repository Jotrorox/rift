// Regenerates the artifact-level maven-metadata.xml (and its checksums) of a
// static Maven repository assembled from several releases' Gradle output.
// Each release ships its own single-version metadata; unpacked over each other
// only the last one would survive, so the version lists are rebuilt here.
//
//   node maven.mjs dist/maven
import { createHash } from 'node:crypto';
import { readdir, stat, writeFile } from 'node:fs/promises';
import { join } from 'node:path';

const root = process.argv[2];
if (!root) {
  console.error('usage: node maven.mjs <repository directory>');
  process.exit(2);
}

// Numeric-aware ordering: 0.10.0 after 0.9.1, 1.0.0-rc1 before 1.0.0.
function compareVersions(a, b) {
  const parse = (v) => v.split(/[.-]/).map((part) => (/^\d+$/.test(part) ? Number(part) : part));
  const [x, y] = [parse(a), parse(b)];
  for (let i = 0; i < Math.max(x.length, y.length); i++) {
    if (x[i] === undefined) return typeof y[i] === 'number' ? -1 : 1;
    if (y[i] === undefined) return typeof x[i] === 'number' ? 1 : -1;
    if (x[i] === y[i]) continue;
    if (typeof x[i] === typeof y[i]) return x[i] < y[i] ? -1 : 1;
    return typeof x[i] === 'number' ? 1 : -1;
  }
  return 0;
}

async function directories(path) {
  const entries = await readdir(path, { withFileTypes: true }).catch(() => []);
  return entries.filter((e) => e.isDirectory()).map((e) => e.name);
}

// An artifact directory holds version directories that each contain a .pom.
async function artifacts(path, segments = []) {
  const found = [];
  for (const name of await directories(path)) {
    const child = join(path, name);
    const files = await readdir(child).catch(() => []);
    if (files.some((f) => f.endsWith('.pom'))) {
      found.push({ path, group: segments.slice(0, -1).join('.'), artifact: segments.at(-1) });
      return found;
    }
    found.push(...(await artifacts(child, [...segments, name])));
  }
  return found;
}

async function writeWithChecksums(path, content) {
  await writeFile(path, content);
  for (const algorithm of ['md5', 'sha1', 'sha256', 'sha512']) {
    await writeFile(`${path}.${algorithm}`, createHash(algorithm).update(content).digest('hex'));
  }
}

const unique = new Map();
for (const found of await artifacts(root)) unique.set(found.path, found);
for (const { path, group, artifact } of unique.values()) {
  const versions = [];
  for (const name of await directories(path)) {
    if ((await readdir(join(path, name))).some((f) => f.endsWith('.pom'))) versions.push(name);
  }
  versions.sort(compareVersions);
  const releases = versions.filter((v) => !v.endsWith('-SNAPSHOT'));
  const updated = new Date((await stat(path)).mtime).toISOString().replace(/\D/g, '').slice(0, 14);
  const xml = `<?xml version="1.0" encoding="UTF-8"?>
<metadata>
  <groupId>${group}</groupId>
  <artifactId>${artifact}</artifactId>
  <versioning>
    <latest>${versions.at(-1)}</latest>
    <release>${releases.at(-1) ?? versions.at(-1)}</release>
    <versions>
${versions.map((v) => `      <version>${v}</version>`).join('\n')}
    </versions>
    <lastUpdated>${updated}</lastUpdated>
  </versioning>
</metadata>
`;
  await writeWithChecksums(join(path, 'maven-metadata.xml'), xml);
  console.log(`${group}:${artifact}: ${versions.join(', ')}`);
}
