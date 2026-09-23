import { useCallback, useEffect, useMemo, useState } from 'react';
import { useShallow } from 'zustand/react/shallow';
import {
  DndContext,
  PointerSensor,
  useSensor,
  useSensors,
  type DragEndEvent,
  closestCenter,
} from '@dnd-kit/core';
import {
  SortableContext,
  horizontalListSortingStrategy,
  useSortable,
} from '@dnd-kit/sortable';
import { CSS } from '@dnd-kit/utilities';
import { motion } from 'motion/react';
import { ChevronLeft, ChevronRight, X, BookOpen, Network, FileText, Telescope, Quote, ClipboardCheck, Users } from 'lucide-react';
import { useUIStore, type Tab, type TabEntry } from '../../stores/ui';
import { useAtomsStore } from '../../stores/atoms';
import { useReportsStore } from '../../stores/reports';
import { getTransport } from '../../lib/transport';
import { formatRelativeDate } from '../../lib/date';

const PILL_WIDTH = 156;

/// Module-scoped cache of fetched atom titles keyed by atom id. Populated
/// lazily when the atom isn't already in the loaded atoms store.
const titleCache = new Map<string, string>();
const titleInflight = new Set<string>();

function fetchAtomTitle(atomId: string, onResolved: (title: string) => void): void {
  if (titleCache.has(atomId)) {
    onResolved(titleCache.get(atomId)!);
    return;
  }
  if (titleInflight.has(atomId)) return;
  titleInflight.add(atomId);
  getTransport()
    .invoke<{ title?: string } | null>('get_atom_by_id', { id: atomId })
    .then((atom) => {
      const title = atom?.title?.trim() || '';
      titleCache.set(atomId, title);
      onResolved(title);
    })
    .catch(() => {
      titleCache.set(atomId, '');
      onResolved('');
    })
    .finally(() => {
      titleInflight.delete(atomId);
    });
}

function useResolvedTitle(entry: TabEntry, ordinal: number, retired = false): { label: string; icon: 'atom' | 'wiki' | 'graph' | 'report' | 'finding' | 'approvals' | 'entities' } {
  const atomFromStore = useAtomsStore(
    useShallow((s) => {
      if (entry.type === 'atom' || entry.type === 'graph') {
        return s.atoms.find((a) => a.id === entry.atomId)?.title ?? null;
      }
      return null;
    })
  );

  // Reports live in their own store; their name is the tab label and
  // is cheap to read here (no extra fetch — reports are listed when
  // the user enters the reports view, and the store keeps `byId`).
  const reportName = useReportsStore(
    useShallow((s) => (entry.type === 'report' ? s.byId[entry.reportId]?.name ?? null : null))
  );

  // For finding tabs, walk the findings cache to recover the parent
  // report's name + creation date so the label reads as a meaningful
  // breadcrumb ("Daily Briefing · 2H AGO"). Cache hits cover everything
  // the user opened via the detail view; a cold deep-link without
  // cache support falls back to entry.title or "Finding".
  const findingMeta = useReportsStore(
    useShallow((s) => {
      if (entry.type !== 'finding') return null;
      for (const [reportId, findings] of Object.entries(s.findingsByReport)) {
        for (const f of findings) {
          if (f.atom.id === entry.atomId) {
            return {
              reportName: s.byId[reportId]?.name ?? f.finding.report_name_snapshot ?? null,
              createdAt: f.atom.created_at,
            };
          }
        }
      }
      return null;
    })
  );

  const [resolved, setResolved] = useState<string | null>(() => {
    if (entry.type === 'atom' || entry.type === 'graph') {
      return titleCache.get(entry.atomId) ?? null;
    }
    return null;
  });

  useEffect(() => {
    if (entry.type === 'wiki' || entry.type === 'report' || entry.type === 'finding' || entry.type === 'tool') return;
    if (atomFromStore && atomFromStore.trim().length > 0) return;
    if (entry.title && entry.title.trim().length > 0) return;
    fetchAtomTitle(entry.atomId, (title) => setResolved(title));
  }, [entry, atomFromStore]);

  if (entry.type === 'wiki') {
    return { label: entry.tagName?.trim() || `Tab ${ordinal}`, icon: 'wiki' };
  }

  if (entry.type === 'report') {
    const label =
      (reportName && reportName.trim()) ||
      (entry.title && entry.title.trim()) ||
      `Report ${ordinal}`;
    return { label, icon: 'report' };
  }

  if (entry.type === 'finding') {
    const name = findingMeta?.reportName?.trim();
    const date = findingMeta?.createdAt
      ? formatRelativeDate(findingMeta.createdAt).toUpperCase()
      : null;
    const label =
      (name && date) ? `${name} · ${date}` :
      (name) ? name :
      (entry.title && entry.title.trim()) ||
      `Finding ${ordinal}`;
    return { label, icon: 'finding' };
  }

  if (entry.type === 'tool') {
    return { label: entry.tool === 'approvals' ? `Approvals${retired ? ' (old)' : ''}` : 'Entities', icon: entry.tool };
  }

  const candidate =
    (atomFromStore && atomFromStore.trim()) ||
    (entry.title && entry.title.trim()) ||
    (resolved && resolved.trim()) ||
    '';
  const label = `${candidate || `Tab ${ordinal}`}${retired ? ' (old)' : ''}`;
  return { label, icon: entry.type === 'graph' ? 'graph' : 'atom' };
}

interface SortablePillProps {
  tab: Tab;
  isActive: boolean;
  onSwitch: () => void;
  onClose: (e: React.MouseEvent) => void;
  onBack: () => void;
  onForward: () => void;
  onMouseDown: (e: React.MouseEvent) => void;
}

function SortablePill({ tab, isActive, onSwitch, onClose, onBack, onForward, onMouseDown }: SortablePillProps) {
  const { attributes, listeners, setNodeRef, transform, transition, isDragging } = useSortable({ id: tab.id });

  const entry = tab.stack[tab.stackIndex];
  const { label, icon } = useResolvedTitle(entry, tab.ordinal, tab.retired);

  const canBack = tab.stackIndex > 0;
  const canForward = tab.stackIndex < tab.stack.length - 1;

  const Icon =
    icon === 'wiki' ? BookOpen :
    icon === 'graph' ? Network :
    icon === 'report' ? Telescope :
    icon === 'finding' ? Quote :
    icon === 'approvals' ? ClipboardCheck :
    icon === 'entities' ? Users :
    FileText;

  const style: React.CSSProperties = {
    transform: CSS.Transform.toString(transform),
    transition,
    width: PILL_WIDTH,
    opacity: isDragging ? 0.5 : 1,
    zIndex: isDragging ? 10 : isActive ? 2 : 1,
  };

  return (
    <div
      ref={setNodeRef}
      style={style}
      {...attributes}
      {...listeners}
      onMouseDown={onMouseDown}
      onClick={onSwitch}
      className={`
        group relative flex items-center h-7 rounded-md cursor-pointer select-none flex-shrink-0
        ${isActive
          ? 'text-white'
          : 'bg-[var(--color-bg-card)] border border-[var(--color-border)] text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] hover:border-[var(--color-border-hover)] transition-colors'}
      `}
      title={label}
    >
      {/* Selection blob — a single shared element across all pills.
          Motion's `layoutId` makes it animate from the previous active pill
          to the new one with a spring. Sits behind the pill content via
          z-index. */}
      {isActive && (
        <motion.div
          layoutId="active-tab-blob"
          className="absolute inset-0 bg-[var(--color-accent)] rounded-md shadow-sm"
          transition={{ type: 'spring', stiffness: 520, damping: 32, mass: 0.9 }}
        />
      )}

      {isActive && (
        <button
          onClick={(e) => {
            e.stopPropagation();
            if (canBack) onBack();
          }}
          onMouseDown={(e) => e.stopPropagation()}
          disabled={!canBack}
          className={`relative z-[1] flex items-center justify-center w-5 h-5 ml-1 rounded transition-colors ${
            canBack ? 'text-white/80 hover:text-white hover:bg-white/15' : 'text-white/30 cursor-default'
          }`}
          title="Back"
          aria-label="Back"
        >
          <ChevronLeft className="w-3.5 h-3.5" strokeWidth={2.5} />
        </button>
      )}

      <div className={`relative z-[1] flex items-center gap-1.5 min-w-0 flex-1 px-2 ${isActive ? '' : 'pl-2.5'}`}>
        <Icon
          className={`w-3 h-3 flex-shrink-0 ${isActive ? 'text-white/80' : 'text-[var(--color-text-tertiary)]'}`}
          strokeWidth={2}
        />
        <span className="text-xs font-medium truncate min-w-0">{label}</span>
      </div>

      {isActive && (
        <button
          onClick={(e) => {
            e.stopPropagation();
            if (canForward) onForward();
          }}
          onMouseDown={(e) => e.stopPropagation()}
          disabled={!canForward}
          className={`relative z-[1] flex items-center justify-center w-5 h-5 rounded transition-colors ${
            canForward ? 'text-white/80 hover:text-white hover:bg-white/15' : 'text-white/30 cursor-default'
          }`}
          title="Forward"
          aria-label="Forward"
        >
          <ChevronRight className="w-3.5 h-3.5" strokeWidth={2.5} />
        </button>
      )}

      {/* Close — appears on hover (or always on the active tab once opened
          to a depth where back/forward are visible, to keep the layout
          stable). On touch we always reserve space. */}
      <button
        onClick={onClose}
        onMouseDown={(e) => e.stopPropagation()}
        className={`relative z-[1] flex items-center justify-center w-5 h-5 mr-1 rounded transition-all ${
          isActive
            ? 'text-white/70 hover:text-white hover:bg-white/15 opacity-100'
            : 'opacity-0 group-hover:opacity-100 text-[var(--color-text-tertiary)] hover:text-[var(--color-text-primary)] hover:bg-[var(--color-bg-hover)]'
        }`}
        title="Close tab"
        aria-label="Close tab"
      >
        <X className="w-3 h-3" strokeWidth={2.5} />
      </button>
    </div>
  );
}

export function TabStrip() {
  const { tabs, activeTabId } = useUIStore(
    useShallow((s) => ({ tabs: s.tabs, activeTabId: s.activeTabId }))
  );
  const switchToTab = useUIStore((s) => s.switchToTab);
  const closeTab = useUIStore((s) => s.closeTab);
  const reorderTabs = useUIStore((s) => s.reorderTabs);
  const tabBack = useUIStore((s) => s.tabBack);
  const tabForward = useUIStore((s) => s.tabForward);

  // PointerSensor with 6px activation distance: a small drag starts the
  // sort, but a plain click goes through to the pill's click handler. The
  // chevrons + close button stop propagation in their own onMouseDown so a
  // drag started on those still doesn't kick off a sort.
  const sensors = useSensors(
    useSensor(PointerSensor, {
      activationConstraint: { distance: 6 },
    })
  );

  const handleDragEnd = useCallback(
    (event: DragEndEvent) => {
      const { active, over } = event;
      if (!over || active.id === over.id) return;
      const fromIndex = tabs.findIndex((t) => t.id === active.id);
      const toIndex = tabs.findIndex((t) => t.id === over.id);
      if (fromIndex === -1 || toIndex === -1) return;
      reorderTabs(fromIndex, toIndex);
    },
    [tabs, reorderTabs]
  );

  // Middle-click closes a tab — same affordance as browser tabs.
  const handlePillMouseDown = useCallback(
    (tabId: string) => (e: React.MouseEvent) => {
      if (e.button === 1) {
        e.preventDefault();
        e.stopPropagation();
        closeTab(tabId);
      }
    },
    [closeTab]
  );

  const tabIds = useMemo(() => tabs.map((t) => t.id), [tabs]);

  if (tabs.length === 0) return null;

  return (
    <div className="flex items-center gap-1 overflow-x-auto scrollbar-auto-hide min-w-0 max-w-full py-0.5">
      <DndContext sensors={sensors} collisionDetection={closestCenter} onDragEnd={handleDragEnd}>
        <SortableContext items={tabIds} strategy={horizontalListSortingStrategy}>
          {/* The parent (MainView) wraps the whole nav cluster in a
              LayoutGroup so the active blob can FLIP between main-nav
              buttons and tab pills. */}
          <div className="flex items-center gap-1">
            {tabs.map((tab) => (
              <SortablePill
                key={tab.id}
                tab={tab}
                isActive={tab.id === activeTabId}
                onSwitch={() => switchToTab(tab.id)}
                onClose={(e) => {
                  e.stopPropagation();
                  closeTab(tab.id);
                }}
                onBack={tabBack}
                onForward={tabForward}
                onMouseDown={handlePillMouseDown(tab.id)}
              />
            ))}
          </div>
        </SortableContext>
      </DndContext>
    </div>
  );
}
