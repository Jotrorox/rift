// Small helpers for the page templates.
export { escapeHtml } from './content.mjs';

export function neighbours(docs, doc) {
  const i = docs.findIndex((d) => d.slug === doc.slug);
  return { prev: docs[i - 1], next: docs[i + 1] };
}

export function readingMinutes(doc) {
  return Math.max(1, Math.round(doc.words / 230));
}

// Pixel-art helper: rows of characters become crisp SVG rectangles.
// `palette` maps characters to fill colours; '.' is transparent.
export function pixelSvg(rows, palette, { size = 16, label = '' } = {}) {
  const h = rows.length;
  const w = rows[0].length;
  let rects = '';
  rows.forEach((row, y) => {
    [...row].forEach((c, x) => {
      if (palette[c]) rects += `<rect x="${x}" y="${y}" width="1" height="1" fill="${palette[c]}"/>`;
    });
  });
  const a11y = label ? `role="img" aria-label="${label}"` : 'aria-hidden="true"';
  return `<svg ${a11y} viewBox="0 0 ${w} ${h}" width="${size}" height="${size * h / w}" shape-rendering="crispEdges">${rects}</svg>`;
}
