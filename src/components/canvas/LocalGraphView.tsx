import { useEffect, useState, useRef } from 'react';
import Graph from 'graphology';
import Sigma from 'sigma';
import EdgeCurveProgram from '@sigma/edge-curve';
import { getAtomNeighborhood, type NeighborhoodGraph, type NeighborhoodEdge } from '../../lib/api';
import { useUIStore } from '../../stores/ui';
import { DEFAULT_THEME, type CanvasTheme } from './sigma/themes';

const RADIUS_DEPTH_1 = 280;
const RADIUS_DEPTH_2 = 540;
const SIZE_CENTER = 18;
const SIZE_DEPTH_1 = 11;
const SIZE_DEPTH_2 = 7;

/**
 * Kill a Sigma instance and proactively release its WebGL contexts.
 *
 * `sigma.kill()` detaches its canvases but the browser may hold the underlying
 * WebGL contexts until GC. Browsers cap concurrent contexts per tab (~16 on
 * Chrome, ~8 on Safari), and rapid re-centers of the local graph can exhaust
 * the pool — the next `new Sigma` then gets a canvas whose `getContext`
 * returns null and crashes on its first GL call (e.g. `gl.blendFunc`).
 * `WEBGL_lose_context.loseContext()` frees the slot immediately.
 */
function releaseCanvasWebGlContext(canvas: HTMLCanvasElement) {
  const gl =
    (canvas.getContext('webgl2') as WebGL2RenderingContext | null) ??
    (canvas.getContext('webgl') as WebGLRenderingContext | null);
  gl?.getExtension('WEBGL_lose_context')?.loseContext();
}

function releaseCanvases(canvases: HTMLCanvasElement[]) {
  for (const canvas of canvases) {
    releaseCanvasWebGlContext(canvas);
  }
}

function releaseSigma(sigma: Sigma, container: HTMLElement) {
  const canvases = Array.from(container.querySelectorAll('canvas'));
  sigma.kill();
  releaseCanvases(canvases);
}

function releasePartialSigmaCanvases(container: HTMLElement, existingCanvases: Set<HTMLCanvasElement>) {
  const partialCanvases = Array.from(container.querySelectorAll('canvas')).filter(
    canvas => !existingCanvases.has(canvas)
  );
  releaseCanvases(partialCanvases);
  for (const canvas of partialCanvases) {
    canvas.remove();
  }
}

/** Brighten/dim an [r,g,b] triple by a 0..1 factor. */
function modulateRgb(rgb: [number, number, number], factor: number): string {
  return `rgb(${Math.round(rgb[0] * factor)},${Math.round(rgb[1] * factor)},${Math.round(rgb[2] * factor)})`;
}

function rgbString(rgb: [number, number, number]): string {
  return `rgb(${rgb[0]},${rgb[1]},${rgb[2]})`;
}

function parseRgbColor(s: string): [number, number, number] | null {
  const m = s.match(/^rgb\((\d+)\s*,\s*(\d+)\s*,\s*(\d+)\)$/);
  if (!m) return null;
  return [+m[1], +m[2], +m[3]];
}

function lerpRgb(a: [number, number, number], b: [number, number, number], t: number): string {
  return `rgb(${Math.round(a[0] + (b[0] - a[0]) * t)},${Math.round(a[1] + (b[1] - a[1]) * t)},${Math.round(a[2] + (b[2] - a[2]) * t)})`;
}

/** Edge styling per type — distinct color AND line weight, so the three are unmistakable.
 *  - tag: muted neutral, thinnest
 *  - semantic: theme accent at full strength, medium
 *  - both: theme accent lifted toward white, thickest (the strongest signal). */
function neighborhoodEdgeStyle(theme: CanvasTheme, edge: NeighborhoodEdge): { color: string; size: number } {
  const s = edge.strength;
  switch (edge.edge_type) {
    case 'semantic':
      return { color: rgbString(theme.edgeMax), size: 0.9 + s * 1.4 };
    case 'both':
      return { color: lerpRgb(theme.edgeMax, [255, 255, 255], 0.45), size: 1.5 + s * 1.8 };
    case 'tag':
    default:
      return { color: lerpRgb(theme.edgeMin, [110, 110, 120], 0.7), size: 0.5 + s * 0.9 };
  }
}

/** Title from atom content's first line, with markdown noise stripped. */
function atomTitle(content: string): string {
  return (
    content.split('\n')[0]
      .replace(/^#+\s*/, '')
      .replace(/\*\*/g, '')
      .replace(/\*/g, '')
      .trim() || 'Untitled'
  );
}

/**
 * Deterministic concentric layout. Center at origin; depth-1 around an inner ring
 * sorted by similarity to center; depth-2 fanned around their depth-1 anchor.
 *
 * No physics, no jiggle — small ego-networks read better when geometry is intentional.
 */
function computeLayout(graph: NeighborhoodGraph): Record<string, { x: number; y: number }> {
  const positions: Record<string, { x: number; y: number }> = {};
  positions[graph.center_atom_id] = { x: 0, y: 0 };

  const depth1 = graph.atoms.filter(a => a.depth === 1);
  const depth2 = graph.atoms.filter(a => a.depth === 2);

  // Edge from center → depth-1 (used for ordering depth-1 around the ring)
  const edgesFromCenter = new Map<string, NeighborhoodEdge>();
  for (const edge of graph.edges) {
    if (edge.source_id === graph.center_atom_id) {
      edgesFromCenter.set(edge.target_id, edge);
    } else if (edge.target_id === graph.center_atom_id) {
      edgesFromCenter.set(edge.source_id, edge);
    }
  }

  // Sort depth-1 by similarity (or strength if no similarity), then by id for stability
  depth1.sort((a, b) => {
    const ea = edgesFromCenter.get(a.id);
    const eb = edgesFromCenter.get(b.id);
    const sa = ea?.similarity_score ?? ea?.strength ?? 0;
    const sb = eb?.similarity_score ?? eb?.strength ?? 0;
    if (sb !== sa) return sb - sa;
    return a.id.localeCompare(b.id);
  });

  // Place depth-1 evenly around an inner ring, starting at top (-π/2) going clockwise
  const angleStep1 = depth1.length > 0 ? (Math.PI * 2) / depth1.length : 0;
  const startAngle = -Math.PI / 2;
  const depth1Angles: Record<string, number> = {};
  for (let i = 0; i < depth1.length; i++) {
    const angle = startAngle + i * angleStep1;
    depth1Angles[depth1[i].id] = angle;
    positions[depth1[i].id] = {
      x: Math.cos(angle) * RADIUS_DEPTH_1,
      y: Math.sin(angle) * RADIUS_DEPTH_1,
    };
  }

  // For each depth-2, find its depth-1 parents (edges that connect them)
  const depth1Set = new Set(depth1.map(a => a.id));
  const depth2Set = new Set(depth2.map(a => a.id));
  const parentEdgeStrength = new Map<string, Map<string, number>>(); // depth2 id → (parent id → strength)
  for (const edge of graph.edges) {
    let d2: string | null = null;
    let d1: string | null = null;
    if (depth2Set.has(edge.source_id) && depth1Set.has(edge.target_id)) {
      d2 = edge.source_id;
      d1 = edge.target_id;
    } else if (depth2Set.has(edge.target_id) && depth1Set.has(edge.source_id)) {
      d2 = edge.target_id;
      d1 = edge.source_id;
    }
    if (d2 && d1) {
      if (!parentEdgeStrength.has(d2)) parentEdgeStrength.set(d2, new Map());
      const m = parentEdgeStrength.get(d2)!;
      m.set(d1, Math.max(m.get(d1) ?? 0, edge.strength));
    }
  }

  // Group depth-2 by primary parent (strongest edge → most stable anchor)
  const childrenByParent = new Map<string, string[]>();
  const orphanDepth2: string[] = [];
  for (const a of depth2) {
    const parents = parentEdgeStrength.get(a.id);
    if (!parents || parents.size === 0) {
      orphanDepth2.push(a.id);
      continue;
    }
    let bestParent = '';
    let bestStrength = -Infinity;
    for (const [pid, s] of parents) {
      if (s > bestStrength || (s === bestStrength && pid < bestParent)) {
        bestParent = pid;
        bestStrength = s;
      }
    }
    if (!childrenByParent.has(bestParent)) childrenByParent.set(bestParent, []);
    childrenByParent.get(bestParent)!.push(a.id);
  }

  // Fan children around their parent's angle on the outer ring
  for (const [parentId, children] of childrenByParent) {
    children.sort();
    const parentAngle = depth1Angles[parentId];
    const fanWidth = Math.min(angleStep1 * 0.7, Math.PI / 2.5);
    const startFan = parentAngle - fanWidth / 2;
    const step = children.length > 1 ? fanWidth / (children.length - 1) : 0;
    for (let i = 0; i < children.length; i++) {
      const angle = children.length === 1 ? parentAngle : startFan + i * step;
      positions[children[i]] = {
        x: Math.cos(angle) * RADIUS_DEPTH_2,
        y: Math.sin(angle) * RADIUS_DEPTH_2,
      };
    }
  }

  // Orphans (no parent edge — shouldn't normally happen) get distributed evenly
  for (let i = 0; i < orphanDepth2.length; i++) {
    const angle = (i / Math.max(1, orphanDepth2.length)) * Math.PI * 2;
    positions[orphanDepth2[i]] = {
      x: Math.cos(angle) * RADIUS_DEPTH_2,
      y: Math.sin(angle) * RADIUS_DEPTH_2,
    };
  }

  return positions;
}

export function LocalGraphView() {
  const localGraph = useUIStore(s => s.localGraph);
  const navigateLocalGraph = useUIStore(s => s.navigateLocalGraph);
  const overlayNavigate = useUIStore(s => s.overlayNavigate);

  const [graph, setGraph] = useState<NeighborhoodGraph | null>(null);
  const [isLoading, setIsLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const containerRef = useRef<HTMLDivElement>(null);
  const sigmaRef = useRef<Sigma | null>(null);
  const graphRef = useRef<Graph | null>(null);

  // Hover state — read by the node/edge reducers (refs so we don't recreate sigma on every change)
  const hoveredNodeRef = useRef<string | null>(null);
  const neighborsRef = useRef<Map<string, Set<string>>>(new Map());
  const hoverAnimRef = useRef(0);
  const hoverTargetRef = useRef(0);

  // Theme is fixed for now — main canvas owns the theme picker. Could be lifted into a shared
  // store later so both views stay in sync.
  const theme = DEFAULT_THEME;
  const themeRef = useRef(theme);
  themeRef.current = theme;

  // Stable refs to navigation handlers so the sigma effect doesn't re-run when they change
  const localGraphRef = useRef(localGraph);
  localGraphRef.current = localGraph;
  const navigateLocalGraphRef = useRef(navigateLocalGraph);
  navigateLocalGraphRef.current = navigateLocalGraph;
  const overlayNavigateRef = useRef(overlayNavigate);
  overlayNavigateRef.current = overlayNavigate;

  // Fetch neighborhood data
  useEffect(() => {
    if (!localGraph.centerAtomId) return;
    let cancelled = false;
    setIsLoading(true);
    setError(null);
    getAtomNeighborhood(localGraph.centerAtomId, localGraph.depth, 0.5)
      .then(data => {
        if (!cancelled) setGraph(data);
      })
      .catch(err => {
        if (!cancelled) setError(err instanceof Error ? err.message : 'Failed to load neighborhood');
      })
      .finally(() => {
        if (!cancelled) setIsLoading(false);
      });
    return () => { cancelled = true; };
  }, [localGraph.centerAtomId, localGraph.depth]);

  // Build / rebuild sigma when graph data arrives
  useEffect(() => {
    const container = containerRef.current;
    if (!container || !graph || graph.atoms.length === 0) return;

    if (sigmaRef.current) {
      releaseSigma(sigmaRef.current, container);
      sigmaRef.current = null;
    }

    const g = new Graph();
    graphRef.current = g;
    const t = themeRef.current;
    const positions = computeLayout(graph);

    // Color each depth-1 atom with its own palette slot so its depth-2 children can
    // inherit the same hue — gives the eye an "orbit" to follow per branch.
    const depth1List = graph.atoms.filter(a => a.depth === 1).map(a => a.id);
    // Match the layout's stable ordering so colors line up with positions
    depth1List.sort((a, b) => {
      const angleA = Math.atan2(positions[a]?.y ?? 0, positions[a]?.x ?? 0);
      const angleB = Math.atan2(positions[b]?.y ?? 0, positions[b]?.x ?? 0);
      return angleA - angleB;
    });
    const paletteIndex = new Map<string, number>();
    depth1List.forEach((id, i) => paletteIndex.set(id, i % t.palette.length));

    // Depth-2 inherits its primary parent's color
    const parentOf = new Map<string, string>();
    const depth1Set = new Set(depth1List);
    const depth2Set = new Set(graph.atoms.filter(a => a.depth === 2).map(a => a.id));
    const candidateParent = new Map<string, { id: string; strength: number }>();
    for (const edge of graph.edges) {
      let d2: string | null = null;
      let d1: string | null = null;
      if (depth2Set.has(edge.source_id) && depth1Set.has(edge.target_id)) {
        d2 = edge.source_id; d1 = edge.target_id;
      } else if (depth2Set.has(edge.target_id) && depth1Set.has(edge.source_id)) {
        d2 = edge.target_id; d1 = edge.source_id;
      }
      if (d2 && d1) {
        const cur = candidateParent.get(d2);
        if (!cur || edge.strength > cur.strength) {
          candidateParent.set(d2, { id: d1, strength: edge.strength });
        }
      }
    }
    for (const [d2, p] of candidateParent) parentOf.set(d2, p.id);

    // Add nodes
    for (const atom of graph.atoms) {
      const pos = positions[atom.id] ?? { x: 0, y: 0 };
      let size: number;
      let color: string;

      if (atom.depth === 0) {
        size = SIZE_CENTER;
        color = rgbString(t.nodeMax);
      } else if (atom.depth === 1) {
        size = SIZE_DEPTH_1;
        const idx = paletteIndex.get(atom.id) ?? 0;
        color = modulateRgb(t.palette[idx], 0.95);
      } else {
        size = SIZE_DEPTH_2;
        const parent = parentOf.get(atom.id);
        const idx = parent !== undefined ? (paletteIndex.get(parent) ?? 0) : 0;
        // Dimmer than the depth-1 anchor so the eye reads them as secondary
        color = modulateRgb(t.palette[idx], 0.55);
      }

      g.addNode(atom.id, {
        x: pos.x,
        y: pos.y,
        size,
        color,
        depth: atom.depth,
        label: atomTitle(atom.content),
        primaryTagName: atom.tags[0]?.name ?? null,
        extraTagCount: Math.max(0, atom.tags.length - 1),
      });
    }

    // Build edges + neighbor map
    const neighbors = new Map<string, Set<string>>();
    for (const edge of graph.edges) {
      if (!g.hasNode(edge.source_id) || !g.hasNode(edge.target_id)) continue;
      if (g.hasEdge(edge.source_id, edge.target_id) || g.hasEdge(edge.target_id, edge.source_id)) continue;
      const style = neighborhoodEdgeStyle(t, edge);
      g.addEdge(edge.source_id, edge.target_id, {
        type: 'curved',
        weight: edge.strength,
        edgeType: edge.edge_type,
        color: style.color,
        size: style.size,
      });
      if (!neighbors.has(edge.source_id)) neighbors.set(edge.source_id, new Set());
      if (!neighbors.has(edge.target_id)) neighbors.set(edge.target_id, new Set());
      neighbors.get(edge.source_id)!.add(edge.target_id);
      neighbors.get(edge.target_id)!.add(edge.source_id);
    }
    neighborsRef.current = neighbors;

    let sigma: Sigma;
    const existingCanvases = new Set(container.querySelectorAll('canvas'));
    try {
      sigma = new Sigma(g, container, {
        // Labels are rendered by our overlay canvas (always-on with collision avoidance).
        renderLabels: false,
        defaultEdgeColor: '#333',
        defaultNodeColor: '#555',
        defaultEdgeType: 'curved',
        zIndex: true,
        edgeProgramClasses: {
          curved: EdgeCurveProgram,
        },
        minCameraRatio: 0.2,
        maxCameraRatio: 4,
        zoomingRatio: 1.03,
        stagePadding: 80,
        defaultDrawNodeHover: () => {}, // Hover ring/pill drawn on overlay
        nodeReducer: (node, attrs) => {
          const hovered = hoveredNodeRef.current;
          if (!hovered) return attrs;
          if (node === hovered) return { ...attrs, zIndex: 2 };
          const isNeighbor = neighborsRef.current.get(hovered)?.has(node);
          if (isNeighbor) return { ...attrs, zIndex: 1 };
          // Non-neighbors fade toward gray. Sizes stay put — in a small ego-network shrinking
          // most of the nodes at once reads as "the whole graph just got smaller", which is
          // disorienting. Color fade is enough to direct attention.
          const dim = hoverAnimRef.current;
          const rgb = parseRgbColor(attrs.color as string);
          const color = rgb
            ? `rgb(${Math.round(rgb[0] + (60 - rgb[0]) * dim)},${Math.round(rgb[1] + (60 - rgb[1]) * dim)},${Math.round(rgb[2] + (60 - rgb[2]) * dim)})`
            : attrs.color;
          return { ...attrs, color };
        },
        edgeReducer: (edge, attrs) => {
          const hovered = hoveredNodeRef.current;
          if (!hovered) return attrs;
          const src = g.source(edge);
          const dst = g.target(edge);
          const incident = src === hovered || dst === hovered;
          const dim = hoverAnimRef.current;
          if (incident) {
            // Brighten incident edges via color (no size pump — same reason as nodeReducer).
            return { ...attrs, zIndex: 1 };
          }
          // Fade non-incident edges toward the background (color only).
          const rgb = parseRgbColor(attrs.color as string);
          const bg = parseRgbColor(themeRef.current.background) ?? [30, 30, 30];
          const color = rgb
            ? `rgb(${Math.round(rgb[0] + (bg[0] - rgb[0]) * dim * 0.85)},${Math.round(rgb[1] + (bg[1] - rgb[1]) * dim * 0.85)},${Math.round(rgb[2] + (bg[2] - rgb[2]) * dim * 0.85)})`
            : attrs.color;
          return { ...attrs, color };
        },
      });
    } catch (err) {
      // WebGL context unavailable — typically the browser has hit its per-tab
      // context limit. Surface a graceful error instead of letting the crash
      // bubble up to the route-level Error Boundary and blank the page.
      console.error('LocalGraphView: failed to initialize graph renderer', err);
      releasePartialSigmaCanvases(container, existingCanvases);
      setError('Could not initialize the graph renderer. Try closing other tabs that use graph or 3D views, or reload the page.');
      graphRef.current = null;
      return;
    }

    sigmaRef.current = sigma;

    // === Label overlay ===
    const labelCanvas = document.createElement('canvas');
    labelCanvas.style.position = 'absolute';
    labelCanvas.style.inset = '0';
    labelCanvas.style.pointerEvents = 'none';
    labelCanvas.style.zIndex = '10';
    container.appendChild(labelCanvas);

    function drawLabels() {
      const width = container!.clientWidth;
      const height = container!.clientHeight;
      const ratio = window.devicePixelRatio || 1;
      labelCanvas.width = width * ratio;
      labelCanvas.height = height * ratio;
      labelCanvas.style.width = `${width}px`;
      labelCanvas.style.height = `${height}px`;

      const ctx = labelCanvas.getContext('2d');
      if (!ctx) return;
      ctx.setTransform(ratio, 0, 0, ratio, 0, 0);
      ctx.clearRect(0, 0, width, height);

      const t = themeRef.current;
      const placed: { x: number; y: number; w: number; h: number }[] = [];
      const collides = (rect: { x: number; y: number; w: number; h: number }, pad: number) => {
        for (const p of placed) {
          if (
            rect.x - pad < p.x + p.w &&
            rect.x + rect.w + pad > p.x &&
            rect.y - pad < p.y + p.h &&
            rect.y + rect.h + pad > p.y
          ) return true;
        }
        return false;
      };

      // Draw labels for each node — center first (highest priority), then by size desc
      type Cand = { id: string; vx: number; vy: number; rsize: number; depth: number; label: string; tag: string | null; extra: number };
      const cands: Cand[] = [];
      g!.forEachNode((id, attrs) => {
        const pos = sigma!.graphToViewport({ x: attrs.x as number, y: attrs.y as number });
        if (pos.x < -300 || pos.x > width + 300 || pos.y < -100 || pos.y > height + 100) return;
        cands.push({
          id,
          vx: pos.x,
          vy: pos.y,
          rsize: sigma!.scaleSize(attrs.size as number),
          depth: (attrs as any).depth as number,
          label: (attrs.label as string) ?? '',
          tag: ((attrs as any).primaryTagName as string | null) ?? null,
          extra: ((attrs as any).extraTagCount as number) ?? 0,
        });
      });
      cands.sort((a, b) => {
        if (a.depth !== b.depth) return a.depth - b.depth; // center first, then 1, then 2
        return b.rsize - a.rsize;
      });

      for (const c of cands) {
        const isCenter = c.depth === 0;
        const fontSize = isCenter ? 14 : c.depth === 1 ? 12 : 11;
        const labelY = c.vy + c.rsize + fontSize / 2 + 6;

        ctx.font = `${isCenter ? 600 : 500} ${fontSize}px system-ui, -apple-system, sans-serif`;
        ctx.textAlign = 'center';
        ctx.textBaseline = 'middle';
        const maxLabelChars = isCenter ? 56 : c.depth === 1 ? 38 : 28;
        const labelText = c.label.length > maxLabelChars ? c.label.substring(0, maxLabelChars - 1) + '…' : c.label;

        const tw = ctx.measureText(labelText).width;
        const padX = isCenter ? 10 : 8;
        const padY = isCenter ? 5 : 4;
        const pillW = tw + padX * 2;
        const pillH = fontSize + padY * 2;
        const rect = { x: c.vx - pillW / 2, y: labelY - pillH / 2, w: pillW, h: pillH };

        // Center always draws even if it collides; everyone else respects collisions
        if (!isCenter && collides(rect, 6)) continue;
        placed.push(rect);

        // Pill background — slightly more opaque for the center
        ctx.fillStyle = isCenter ? t.labelBg : t.labelBg.replace('rgb', 'rgba').replace(')', ',0.92)');
        ctx.beginPath();
        ctx.roundRect(rect.x, rect.y, pillW, pillH, pillH / 2);
        ctx.fill();
        ctx.strokeStyle = isCenter ? 'rgba(255,255,255,0.35)' : t.labelBorder;
        ctx.lineWidth = isCenter ? 1.5 : 1;
        ctx.stroke();

        // Title
        ctx.fillStyle = isCenter ? '#f0f0f0' : t.nodeLabelColor;
        ctx.fillText(labelText, c.vx, labelY);

        // Tag rendered as plain caption below the title pill — no second pill, no border.
        // Letter-spaced uppercase reads as metadata, not as a competing label.
        if (c.tag && c.depth <= 1) {
          const tagFont = 9;
          const tagShort = c.tag.length > 18 ? c.tag.substring(0, 17) + '…' : c.tag;
          const display = (c.extra > 0 ? `${tagShort}  +${c.extra}` : tagShort).toUpperCase();
          ctx.font = `500 ${tagFont}px system-ui, -apple-system, sans-serif`;
          ctx.textAlign = 'center';
          ctx.textBaseline = 'middle';
          // Manual letter-spacing for that editorial micro-caps look
          const spacing = 0.6;
          let totalW = 0;
          for (const ch of display) totalW += ctx.measureText(ch).width + spacing;
          totalW -= spacing;
          const cy = labelY + pillH / 2 + 8 + tagFont / 2;
          const cRect = { x: c.vx - totalW / 2, y: cy - tagFont / 2, w: totalW, h: tagFont };
          if (!collides(cRect, 4)) {
            placed.push(cRect);
            ctx.fillStyle = t.nodeLabelColor + '99'; // ~60% alpha if hex; harmless on rgb()
            // Fallback for rgb() colors — fade via globalAlpha
            const wasAlpha = ctx.globalAlpha;
            ctx.globalAlpha = wasAlpha * 0.65;
            let cx = c.vx - totalW / 2;
            for (const ch of display) {
              const cw = ctx.measureText(ch).width;
              ctx.fillStyle = t.nodeLabelColor;
              ctx.textAlign = 'left';
              ctx.fillText(ch, cx, cy);
              cx += cw + spacing;
            }
            ctx.globalAlpha = wasAlpha;
          }
        }
      }

      // Hover ring — paints last so it sits above labels
      const hoveredId = hoveredNodeRef.current;
      const hAnim = hoverAnimRef.current;
      if (hoveredId && hAnim > 0.01 && g!.hasNode(hoveredId)) {
        const hAttrs = g!.getNodeAttributes(hoveredId);
        const hPos = sigma!.graphToViewport({ x: hAttrs.x as number, y: hAttrs.y as number });
        const hSize = sigma!.scaleSize(hAttrs.size as number);
        ctx.globalAlpha = hAnim;
        ctx.beginPath();
        ctx.arc(hPos.x, hPos.y, hSize + 3, 0, Math.PI * 2);
        ctx.strokeStyle = 'rgba(255,255,255,0.55)';
        ctx.lineWidth = 2;
        ctx.stroke();
        ctx.globalAlpha = 1;
      }
    }

    sigma.on('afterRender', drawLabels);
    requestAnimationFrame(drawLabels);

    // No setCustomBBox: with a padded custom bbox, the first render fits to the
    // natural bbox before our setCustomBBox call lands, then the next refresh re-fits
    // to the larger box and the whole graph appears to "zoom out" on first hover.
    // Sigma's natural bbox is consistent across renders — let stagePadding handle margin.

    // Hover animation loop — exponential ease toward target, stops when settled
    let hoverRaf: number | null = null;
    const tickHover = () => {
      const diff = hoverTargetRef.current - hoverAnimRef.current;
      if (Math.abs(diff) < 0.005) {
        hoverAnimRef.current = hoverTargetRef.current;
        if (hoverTargetRef.current === 0) hoveredNodeRef.current = null;
        sigma.refresh();
        hoverRaf = null;
        return;
      }
      hoverAnimRef.current += diff * 0.22;
      sigma.refresh();
      hoverRaf = requestAnimationFrame(tickHover);
    };
    const startHoverAnim = () => {
      if (hoverRaf !== null) return;
      hoverRaf = requestAnimationFrame(tickHover);
    };

    sigma.on('enterNode', ({ node }) => {
      hoveredNodeRef.current = node;
      hoverTargetRef.current = 1;
      startHoverAnim();
    });
    sigma.on('leaveNode', () => {
      hoverTargetRef.current = 0;
      startHoverAnim();
    });

    sigma.on('clickNode', ({ node, event }) => {
      const orig = event?.original as MouseEvent | undefined;
      const newTab = !!(orig && (orig.metaKey || orig.ctrlKey));
      const centerId = localGraphRef.current.centerAtomId;
      if (newTab) {
        overlayNavigateRef.current({ type: 'reader', atomId: node }, { newTab: true });
        return;
      }
      // Click center → open in reader; click any other node → recenter graph there
      if (node === centerId) {
        overlayNavigateRef.current({ type: 'reader', atomId: node });
      } else {
        navigateLocalGraphRef.current(node);
      }
    });

    sigma.on('doubleClickNode', ({ node, event }) => {
      const orig = event?.original as MouseEvent | undefined;
      const newTab = !!(orig && (orig.metaKey || orig.ctrlKey));
      // doubleClick always opens reader — preempts the camera double-click-zoom default
      event?.preventSigmaDefault?.();
      overlayNavigateRef.current({ type: 'reader', atomId: node }, { newTab });
    });

    return () => {
      if (hoverRaf !== null) cancelAnimationFrame(hoverRaf);
      releaseSigma(sigma, container);
      labelCanvas.remove();
      sigmaRef.current = null;
      graphRef.current = null;
    };
  }, [graph]);

  if (!localGraph.isOpen) return null;

  return (
    <div className="h-full bg-[var(--color-bg-main)] flex flex-col">
      {/* Graph stage */}
      <div
        className="flex-1 relative overflow-hidden"
        style={{ backgroundColor: theme.background }}
      >
        {isLoading && (
          <div className="absolute inset-0 flex items-center justify-center bg-[var(--color-bg-main)]/80 z-20">
            <div className="text-[var(--color-text-secondary)]">Loading neighborhood…</div>
          </div>
        )}

        {error && (
          <div className="absolute inset-0 flex items-center justify-center bg-[var(--color-bg-main)]/80 z-20">
            <div className="text-red-500">{error}</div>
          </div>
        )}

        {!isLoading && !error && graph && graph.atoms.length <= 1 && (
          <div className="absolute inset-0 flex items-center justify-center pointer-events-none z-10">
            <div className="text-sm text-[var(--color-text-tertiary)]">
              No connected atoms found at this depth.
            </div>
          </div>
        )}

        <div ref={containerRef} className="w-full h-full" />
      </div>

      {/* Legend */}
      <div className="px-4 py-2 border-t border-[var(--color-border)] flex items-center gap-6 text-xs text-[var(--color-text-secondary)]">
        <div className="flex items-center gap-2">
          <div
            className="w-3 h-3 rounded-full"
            style={{ background: rgbString(theme.nodeMax) }}
          />
          <span>Center atom</span>
        </div>
        <div className="flex items-center gap-2">
          <div className="w-6" style={{ height: 1, background: lerpRgb(theme.edgeMin, [110, 110, 120], 0.7) }} />
          <span>Tag connection</span>
        </div>
        <div className="flex items-center gap-2">
          <div className="w-6" style={{ height: 2, background: rgbString(theme.edgeMax) }} />
          <span>Semantic connection</span>
        </div>
        <div className="flex items-center gap-2">
          <div className="w-6" style={{ height: 3, background: lerpRgb(theme.edgeMax, [255, 255, 255], 0.45) }} />
          <span>Both</span>
        </div>
      </div>
    </div>
  );
}
