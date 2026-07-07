import { useMemo, useRef, useState, type MouseEvent as ReactMouseEvent } from 'react';
import { useVirtualizer } from '@tanstack/react-virtual';
import { Search } from 'lucide-react';
import { useCanvasStore } from '../../stores/canvas';
import { CANVAS_NONE_KEY, useUIStore } from '../../stores/ui';
import type { CanvasAtomPosition } from '../../lib/api';

const EMPTY_ATOMS: CanvasAtomPosition[] = [];

export function CanvasEntitiesSection() {
  const viewMode = useUIStore(s => s.viewMode);
  const atoms = useCanvasStore(s => s.canvasData?.atoms ?? EMPTY_ATOMS);
  const canvasEntityVisible = useUIStore(s => s.canvasEntityVisible);
  const setCanvasEntityVisible = useUIStore(s => s.setCanvasEntityVisible);
  const setCanvasEntityVisibleMap = useUIStore(s => s.setCanvasEntityVisibleMap);
  const [showSearch, setShowSearch] = useState(false);
  const [query, setQuery] = useState('');
  const [height, setHeight] = useState(208);
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
  const visibleRows = useMemo(() => {
    const needle = query.trim().toLocaleLowerCase();
    return needle ? rows.filter(([, name]) => name.toLocaleLowerCase().includes(needle)) : rows;
  }, [query, rows]);
  const hasMemuAtoms = useMemo(
    () => atoms.some(atom => atom.atom_id.startsWith('memory:') || atom.atom_id.startsWith('category:')),
    [atoms]
  );
  const virtualizer = useVirtualizer({
    count: visibleRows.length,
    getScrollElement: () => parentRef.current,
    estimateSize: () => 28,
    overscan: 5,
  });

  if (viewMode !== 'canvas' || atoms.length === 0 || (entities.length === 0 && !hasMemuAtoms)) return null;

  const handleResizeStart = (e: ReactMouseEvent<HTMLDivElement>) => {
    e.preventDefault();
    const startY = e.clientY;
    const startHeight = height;
    const onMouseMove = (event: MouseEvent) => {
      setHeight(Math.min(Math.max(startHeight + startY - event.clientY, 96), window.innerHeight * 0.6));
    };
    const onMouseUp = () => {
      document.removeEventListener('mousemove', onMouseMove);
      document.removeEventListener('mouseup', onMouseUp);
      document.body.style.cursor = '';
      document.body.style.userSelect = '';
    };
    document.addEventListener('mousemove', onMouseMove);
    document.addEventListener('mouseup', onMouseUp);
    document.body.style.cursor = 'row-resize';
    document.body.style.userSelect = 'none';
  };

  return (
    <div
      className="relative flex min-h-24 flex-col overflow-hidden border-t border-[var(--color-border)] px-3 py-2 shrink-0"
      style={{ height }}
    >
      <div
        onMouseDown={handleResizeStart}
        className="absolute left-0 right-0 top-0 h-2 cursor-row-resize hover:bg-[var(--color-accent)]/20"
        title="Resize entities panel"
      />
      <div className="mb-2 flex shrink-0 items-center justify-between">
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
          <button
            onClick={() => setShowSearch(open => !open)}
            className="p-1 rounded hover:bg-[var(--color-bg-hover)] text-[var(--color-text-tertiary)] hover:text-[var(--color-text-primary)] transition-colors"
            title="Search entities"
          >
            <Search className="w-4 h-4" strokeWidth={2} />
          </button>
        </div>
      </div>
      {showSearch && (
        <input
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          placeholder="Filter entities..."
          className="mb-2 w-full shrink-0 rounded border border-[var(--color-border)] bg-[var(--color-bg-card)] px-2 py-1 text-xs text-[var(--color-text-primary)] placeholder:text-[var(--color-text-tertiary)] focus:border-[var(--color-accent)] focus:outline-none"
          autoFocus
        />
      )}
      <div ref={parentRef} className="min-h-0 flex-1 overflow-y-auto">
        <div style={{ height: `${virtualizer.getTotalSize()}px`, position: 'relative' }}>
          {virtualizer.getVirtualItems().map((virtualItem) => {
            const [id, name] = visibleRows[virtualItem.index];
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
        {visibleRows.length === 0 && (
          <div className="px-1 py-2 text-xs text-[var(--color-text-tertiary)]">No matching entities</div>
        )}
      </div>
    </div>
  );
}
