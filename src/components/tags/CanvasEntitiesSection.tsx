import { useMemo, useRef } from 'react';
import { useVirtualizer } from '@tanstack/react-virtual';
import { useCanvasStore } from '../../stores/canvas';
import { CANVAS_NONE_KEY, useUIStore } from '../../stores/ui';

export function CanvasEntitiesSection() {
  const viewMode = useUIStore(s => s.viewMode);
  const atoms = useCanvasStore(s => s.canvasData?.atoms ?? []);
  const canvasEntityVisible = useUIStore(s => s.canvasEntityVisible);
  const setCanvasEntityVisible = useUIStore(s => s.setCanvasEntityVisible);
  const setCanvasEntityVisibleMap = useUIStore(s => s.setCanvasEntityVisibleMap);
  const parentRef = useRef<HTMLDivElement>(null);

  const entities = useMemo(() => {
    const byId = new Map<string, string>();
    for (const atom of atoms) {
      for (let i = 0; i < atom.entity_ids.length; i++) {
        byId.set(atom.entity_ids[i], atom.entity_names[i] || atom.entity_ids[i]);
      }
    }
    return [...byId.entries()].sort((a, b) => a[1].localeCompare(b[1]));
  }, [atoms]);

  const rows = useMemo(() => [[CANVAS_NONE_KEY, 'No entities'] as [string, string], ...entities], [entities]);
  const virtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => parentRef.current,
    estimateSize: () => 28,
    overscan: 5,
  });

  if (viewMode !== 'canvas' || atoms.length === 0) return null;

  return (
    <div className="border-t border-[var(--color-border)] px-3 py-2 shrink-0">
      <div className="mb-2 flex items-center justify-between">
        <span className="text-xs font-semibold text-[var(--color-text-tertiary)] uppercase tracking-wider">
          Entities
        </span>
        <div className="flex items-center gap-1">
          <button
            onClick={() => setCanvasEntityVisibleMap({})}
            className="px-1.5 py-0.5 text-[10px] rounded hover:bg-[var(--color-bg-hover)] text-[var(--color-text-tertiary)] hover:text-[var(--color-text-primary)] transition-colors"
            title="Show all entities on canvas"
          >
            all
          </button>
          <button
            onClick={() => setCanvasEntityVisibleMap(Object.fromEntries(rows.map(([id]) => [id, false])))}
            className="px-1.5 py-0.5 text-[10px] rounded hover:bg-[var(--color-bg-hover)] text-[var(--color-text-tertiary)] hover:text-[var(--color-text-primary)] transition-colors"
            title="Hide all entities on canvas"
          >
            none
          </button>
        </div>
      </div>
      <div ref={parentRef} className="max-h-44 overflow-y-auto">
        <div style={{ height: `${virtualizer.getTotalSize()}px`, position: 'relative' }}>
          {virtualizer.getVirtualItems().map((virtualItem) => {
            const [id, name] = rows[virtualItem.index];
            return (
              <label
                key={id}
                className="absolute left-0 right-0 flex items-center gap-2 rounded px-1 py-1 text-sm text-[var(--color-text-secondary)] hover:bg-[var(--color-bg-card)]"
                style={{ top: 0, height: `${virtualItem.size}px`, transform: `translateY(${virtualItem.start}px)` }}
              >
                <input
                  type="checkbox"
                  checked={canvasEntityVisible[id] ?? true}
                  onChange={(e) => setCanvasEntityVisible(id, e.target.checked)}
                  className="h-3 w-3 accent-[var(--color-accent)]"
                />
                <span className="truncate">{name}</span>
              </label>
            );
          })}
        </div>
      </div>
    </div>
  );
}
