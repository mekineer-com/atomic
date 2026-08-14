import { memo, MouseEvent, useCallback } from 'react';
import { BookOpen, ChevronRight, MessageCircle, FileText } from 'lucide-react';
import { TagWithCount, useTagsStore } from '../../stores/tags';
import { useUIStore } from '../../stores/ui';

interface TagNodeProps {
  tag: TagWithCount;
  level: number;
  selectedTagId: string | null;
  onSelect: (tagId: string) => void;
  onContextMenu: (e: MouseEvent, tag: TagWithCount) => void;
  showCanvasCheckbox?: boolean;
  canvasChecked?: boolean;
  onCanvasCheck?: (tagId: string, checked: boolean) => void;
}

export const TagNode = memo(function TagNode({
  tag,
  level,
  selectedTagId,
  onSelect,
  onContextMenu,
  showCanvasCheckbox = false,
  canvasChecked = true,
  onCanvasCheck,
}: TagNodeProps) {
  const openChatSidebar = useUIStore(s => s.openChatSidebar);
  const isExpanded = useUIStore(s => !!s.expandedTagIds[tag.id]);
  const toggleTagExpanded = useUIStore(s => s.toggleTagExpanded);
  const fetchTagChildren = useTagsStore(s => s.fetchTagChildren);
  const hasChildren = (tag.children && tag.children.length > 0) || tag.children_total > 0;
  const isSelected = selectedTagId === tag.id;
  const isMemuGroup = tag.id.startsWith('memu:group:');
  const isMemuCategory = tag.id.startsWith('category:');

  const handleToggle = useCallback(async (e: MouseEvent) => {
    e.stopPropagation();
    // If expanding and there are more children than currently loaded, fetch all
    if (!isExpanded && tag.children_total > tag.children.length) {
      await fetchTagChildren(tag.id);
    }
    toggleTagExpanded(tag.id);
  }, [isExpanded, tag.children_total, tag.children.length, tag.id, fetchTagChildren, toggleTagExpanded]);

  const handleContextMenu = (e: MouseEvent) => {
    e.preventDefault();
    if (isMemuGroup || isMemuCategory) return;
    onContextMenu(e, tag);
  };

  const openWikiReader = useUIStore(s => s.openWikiReader);
  const openReader = useUIStore(s => s.openReader);

  const handleWikiClick = (e: MouseEvent) => {
    e.stopPropagation();
    openWikiReader(tag.id, tag.name, undefined, { newTab: e.metaKey || e.ctrlKey });
  };

  const handleReaderClick = (e: MouseEvent) => {
    e.stopPropagation();
    openReader(tag.id, undefined, { newTab: e.metaKey || e.ctrlKey });
  };

  const handleChatClick = (e: MouseEvent) => {
    e.stopPropagation();
    openChatSidebar(tag.id);
  };

  return (
    <div
      className={`group relative flex items-center gap-1 px-2 py-1.5 rounded-md cursor-pointer transition-colors ${
        isSelected
          ? 'bg-[var(--color-accent)]/20 text-[var(--color-text-primary)]'
          : 'text-[var(--color-text-secondary)] hover:bg-[var(--color-bg-card)] hover:text-[var(--color-text-primary)]'
      } ${isMemuCategory && tag.active === false ? 'opacity-55' : ''}`}
      style={{ paddingLeft: `${8 + level * 16}px` }}
      onClick={(e) => (isMemuGroup || hasChildren) ? handleToggle(e) : onSelect(tag.id)}
      onContextMenu={handleContextMenu}
    >
      {showCanvasCheckbox && (
        <input
          type="checkbox"
          checked={canvasChecked}
          onChange={(e) => {
            e.stopPropagation();
            onCanvasCheck?.(tag.id, e.target.checked);
          }}
          onClick={(e) => e.stopPropagation()}
          className="h-3 w-3 accent-[var(--color-accent)]"
          title="Show on canvas"
        />
      )}
      {hasChildren ? (
        <button
          onClick={handleToggle}
          className="w-4 h-4 flex items-center justify-center text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)]"
        >
          <ChevronRight className={`w-3 h-3 transition-transform ${isExpanded ? 'rotate-90' : ''}`} strokeWidth={2} />
        </button>
      ) : (
        <span className="w-4" />
      )}
      <span className={`flex-1 truncate text-sm ${isMemuGroup ? 'font-semibold' : ''}`}>{tag.name}</span>
      {isMemuCategory && tag.active === false && (
        <span className="text-[10px] text-[var(--color-text-tertiary)]">inactive</span>
      )}
      {!hasChildren && !isMemuGroup && (
        <div className="pointer-events-none absolute right-1 flex items-center rounded-md bg-[var(--color-bg-card)] opacity-0 shadow-sm transition-opacity group-hover:pointer-events-auto group-hover:opacity-100">
          {/* Chat icon - visible on hover */}
          <button
            onClick={handleChatClick}
            className="w-5 h-5 flex items-center justify-center text-[var(--color-text-secondary)] hover:text-[var(--color-accent-light)] transition-colors"
            title="Chat with this tag"
          >
            <MessageCircle className="w-3.5 h-3.5" strokeWidth={2} />
          </button>
          {/* Article icon - visible on hover */}
          <button
            onClick={handleWikiClick}
            className="w-5 h-5 flex items-center justify-center text-[var(--color-text-secondary)] hover:text-[var(--color-accent-light)] transition-colors"
            title="View wiki article"
          >
            <FileText className="w-3.5 h-3.5" strokeWidth={2} />
          </button>
          {isMemuCategory && (
            <button
              onClick={handleReaderClick}
              className="w-5 h-5 flex items-center justify-center text-[var(--color-text-secondary)] hover:text-[var(--color-accent-light)] transition-colors"
              title="Open in reader"
            >
              <BookOpen className="w-3.5 h-3.5" strokeWidth={2} />
            </button>
          )}
        </div>
      )}
      {!hasChildren && !isMemuGroup && <span className="text-xs text-[var(--color-text-tertiary)] tabular-nums">{tag.atom_count}</span>}
    </div>
  );
});
