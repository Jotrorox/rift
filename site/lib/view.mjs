// Small helpers for the page templates.
export { escapeHtml } from './content.mjs';

export function neighbours(docs, doc) {
  const i = docs.findIndex((d) => d.slug === doc.slug);
  return { prev: docs[i - 1], next: docs[i + 1] };
}

export function readingMinutes(doc) {
  return Math.max(1, Math.round(doc.words / 230));
}
