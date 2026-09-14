import type { ViewMode } from '../stores/ui';

/// URL is the source of truth for "what view am I looking at, which tag am I
/// scoped to, and which atom/wiki is open in the reader overlay". Everything
/// else (edit mode, save status, panel widths, chat sidebar open, etc.) stays
/// as UI-only state — it doesn't belong in the URL.
///
/// Tags are identified by opaque ID in URLs (not by human-readable path).
/// Pretty URLs via tag paths would require the tag tree to be loaded before
/// a URL can be parsed, which complicates cold deep-links. ID-in-URL is
/// ugly but unambiguous and robust. We can revisit when the tag tree has a
/// cheap path-lookup on the server.

export type ParsedRoute =
  | { kind: 'view'; viewMode: ViewMode; tagId: string | null }
  | { kind: 'reader'; atomId: string; tagId: string | null }
  | { kind: 'graph'; atomId: string; tagId: string | null }
  | { kind: 'wiki-reader'; tagId: string; tagName: string | null }
  | { kind: 'reports-detail'; reportId: string }
  | { kind: 'finding-reader'; atomId: string }
  | { kind: 'tool'; tool: 'approvals' | 'entities' };

const VIEW_MODES: ViewMode[] = ['dashboard', 'atoms', 'canvas', 'wiki', 'reports'];

/// Build the URL for a base view, preserving the current tag scope.
export function viewPath(mode: ViewMode, tagId?: string | null): string {
  const base = mode === 'dashboard' ? '/' : `/${mode}`;
  return tagId ? `${base}?tag=${encodeURIComponent(tagId)}` : base;
}

/// Build the URL for an open atom reader.
export function atomReaderPath(atomId: string, tagId?: string | null): string {
  const base = `/atoms/${encodeURIComponent(atomId)}`;
  return tagId ? `${base}?tag=${encodeURIComponent(tagId)}` : base;
}

/// Build the URL for the local-graph view centered on an atom.
export function atomGraphPath(atomId: string, tagId?: string | null): string {
  const base = `/atoms/${encodeURIComponent(atomId)}/graph`;
  return tagId ? `${base}?tag=${encodeURIComponent(tagId)}` : base;
}

/// Build the URL for an open wiki reader. The tagName search param is a
/// display-only hint so a cold deep-link can show the tag name in the header
/// without waiting for the tag tree to load.
export function wikiReaderPath(tagId: string, tagName?: string | null): string {
  const base = `/wiki-reader/${encodeURIComponent(tagId)}`;
  return tagName ? `${base}?name=${encodeURIComponent(tagName)}` : base;
}

/// Build the URL for an open report detail view. `/reports/:id` is
/// parsed *before* the `/reports` base view so the more specific match
/// wins.
export function reportDetailPath(reportId: string): string {
  return `/reports/${encodeURIComponent(reportId)}`;
}

/// Build the URL for a finding reader. Findings are atoms with
/// `kind = 'report'`; their dedicated URL keeps the reader specialized
/// (citation popovers, parent-report header) without polluting the
/// `/atoms/:id` route, which still handles captured atoms.
export function findingReaderPath(atomId: string): string {
  return `/findings/${encodeURIComponent(atomId)}`;
}

export function toolPath(tool: 'approvals' | 'entities'): string {
  return `/${tool}`;
}

/// Parse a pathname + search string into one of our known route shapes.
/// Unknown paths fall back to `dashboard` — no dedicated 404 for now.
export function parseLocation(pathname: string, search: string): ParsedRoute {
  const params = new URLSearchParams(search);
  const tagId = params.get('tag');

  // Strip trailing slash except for root.
  const path = pathname !== '/' && pathname.endsWith('/')
    ? pathname.slice(0, -1)
    : pathname;

  if (path === '/approvals' || path === '/entities') {
    return { kind: 'tool', tool: path.slice(1) as 'approvals' | 'entities' };
  }

  // Local-graph overlay: /atoms/<id>/graph  (checked before reader so the
  // more specific path wins).
  const graphMatch = path.match(/^\/atoms\/([^/]+)\/graph$/);
  if (graphMatch) {
    return { kind: 'graph', atomId: decodeURIComponent(graphMatch[1]), tagId };
  }

  // Atom reader overlay: /atoms/<id>
  const atomMatch = path.match(/^\/atoms\/([^/]+)$/);
  if (atomMatch) {
    return { kind: 'reader', atomId: decodeURIComponent(atomMatch[1]), tagId };
  }

  // Wiki reader overlay: /wiki-reader/<tagId>
  const wikiMatch = path.match(/^\/wiki-reader\/([^/]+)$/);
  if (wikiMatch) {
    const name = params.get('name');
    return { kind: 'wiki-reader', tagId: decodeURIComponent(wikiMatch[1]), tagName: name };
  }

  // Report detail overlay: /reports/<id> — checked before the
  // `/reports` base-view match so a deep-link to a specific report
  // doesn't degrade to the list.
  const reportMatch = path.match(/^\/reports\/([^/]+)$/);
  if (reportMatch) {
    return { kind: 'reports-detail', reportId: decodeURIComponent(reportMatch[1]) };
  }

  // Finding reader overlay: /findings/<atom_id>. Findings are atoms
  // with `kind = 'report'`; the dedicated URL signals that the
  // specialized reader (citation popovers, parent-report header)
  // should mount instead of the generic AtomReader.
  const findingMatch = path.match(/^\/findings\/([^/]+)$/);
  if (findingMatch) {
    return { kind: 'finding-reader', atomId: decodeURIComponent(findingMatch[1]) };
  }

  // Base views: /, /atoms, /canvas, /wiki, /reports
  if (path === '/') return { kind: 'view', viewMode: 'dashboard', tagId };
  const modeSegment = path.slice(1); // drop leading '/'
  if (VIEW_MODES.includes(modeSegment as ViewMode)) {
    return { kind: 'view', viewMode: modeSegment as ViewMode, tagId };
  }

  // Fallback: treat as dashboard.
  return { kind: 'view', viewMode: 'dashboard', tagId };
}
