import { createContext, Fragment, useCallback, useContext, useEffect, useMemo, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { BookOpen, FileText, Hash, MessageCircle, Minus, Plus } from 'lucide-react';
import { CommandInput } from '../command-palette/CommandInput';
import { byteOffsetsToUtf16, MATCH_SNIPPET_PAD, useSearchPalette } from './useSearchPalette';
import { MATCH_END, MATCH_START, markdownToPlainText } from './markdownToPlainText';
import {
  GlobalChatSearchResult,
  GlobalTagSearchResult,
  GlobalWikiSearchResult,
  MatchOffset,
  SemanticSearchResult,
} from '../command-palette/types';

// When true, rows disable their :hover background. Set while the user is
// driving via keyboard so rows that slide past the cursor during scroll don't
// flash hover state — cleared on the next real mousemove inside the palette.
const KeyboardNavContext = createContext(false);

function Snippet({ text, compact }: { text: string; compact?: boolean }) {
  const segments = useMemo(() => {
    const plain = markdownToPlainText(text);
    if (!plain.includes(MATCH_START)) {
      return [{ text: plain, match: false }];
    }
    const out: Array<{ text: string; match: boolean }> = [];
    let i = 0;
    while (i < plain.length) {
      const start = plain.indexOf(MATCH_START, i);
      if (start === -1) {
        out.push({ text: plain.slice(i), match: false });
        break;
      }
      if (start > i) {
        out.push({ text: plain.slice(i, start), match: false });
      }
      const end = plain.indexOf(MATCH_END, start + 1);
      if (end === -1) {
        // Dangling start marker — drop it and emit the rest as plain text.
        out.push({ text: plain.slice(start + 1), match: false });
        break;
      }
      out.push({ text: plain.slice(start + 1, end), match: true });
      i = end + 1;
    }
    return out.filter((seg) => seg.text.length > 0);
  }, [text]);

  const baseClass = 'text-xs leading-5 text-[var(--color-text-secondary)]';
  return (
    <div
      className={
        compact
          ? `${baseClass} truncate`
          : `mt-1 h-10 overflow-hidden ${baseClass}`
      }
    >
      {segments.map((seg, idx) =>
        seg.match ? (
          <strong key={idx} className="font-semibold text-[var(--color-text-primary)]">
            {seg.text}
          </strong>
        ) : (
          <Fragment key={idx}>{seg.text}</Fragment>
        ),
      )}
    </div>
  );
}

interface SearchPaletteProps {
  isOpen: boolean;
  onClose: () => void;
  initialQuery?: string;
}

function SectionHeader({ label, count }: { label: string; count: number }) {
  return (
    <div className="px-4 py-1.5 text-xs font-medium text-[var(--color-text-tertiary)] uppercase tracking-wide flex items-center justify-between">
      <span>{label}</span>
      <span className="normal-case font-normal">{count}</span>
    </div>
  );
}

function PaletteItem({
  selected,
  onClick,
  icon,
  title,
  subtitle,
  meta,
  disclosure,
  onDisclosureClick,
  indented,
}: {
  selected: boolean;
  onClick: () => void;
  icon: React.ReactNode;
  title?: string;
  subtitle?: string;
  meta?: string;
  /** Show a +/− toggle to signal expandability. `null`/undefined means not expandable. */
  disclosure?: 'collapsed' | 'expanded' | null;
  /** Click handler for the disclosure toggle itself — stopped from bubbling to the row. */
  onDisclosureClick?: () => void;
  /** Render the row indented and compact (used for match sub-rows under an atom). */
  indented?: boolean;
}) {
  const usingKeyboard = useContext(KeyboardNavContext);
  const unselectedClass =
    'border-l-2 border-transparent' +
    (usingKeyboard ? '' : ' hover:bg-[var(--color-bg-hover)]');
  // Keyboard nav needs the highlight to appear instantly — the default
  // `transition-colors` animates bg-color over ~150ms and visibly lags the
  // scroll, producing a delayed flash on the newly-selected row.
  const transitionClass = usingKeyboard ? '' : 'transition-colors';
  return (
    <div
      role="button"
      onClick={onClick}
      data-palette-selected={selected ? 'true' : undefined}
      className={`w-full flex items-start gap-3 text-left cursor-pointer ${transitionClass} ${
        indented ? 'py-1.5 pl-12 pr-4' : 'py-3 px-4'
      } ${
        selected
          ? 'bg-[var(--color-accent)] border-l-2 border-[var(--color-accent)]'
          : unselectedClass
      }`}
    >
      {icon ? <span className="text-[var(--color-text-secondary)] mt-0.5">{icon}</span> : null}
      <div className="min-w-0 flex-1">
        {title ? (
          <div className="flex items-center gap-2">
            <span className="truncate text-sm font-medium text-[var(--color-text-primary)]">{title}</span>
            {(meta || disclosure) ? (
              <div className="ml-auto flex items-center gap-2 shrink-0 text-[var(--color-text-tertiary)]">
                {meta ? <span className="text-[10px]">{meta}</span> : null}
                {disclosure ? (
                  <button
                    type="button"
                    onClick={(e) => {
                      e.stopPropagation();
                      onDisclosureClick?.();
                    }}
                    // p-1 -m-1 expands the click target without shifting layout —
                    // tapping a 14px icon on a mouse is fine, but this palette also
                    // opens on mobile where a bigger hit area matters.
                    className="p-1 -m-1 rounded hover:text-[var(--color-text-primary)] flex items-center"
                    aria-label={disclosure === 'expanded' ? 'Collapse matches' : 'Expand matches'}
                  >
                    {disclosure === 'expanded' ? (
                      <Minus className="w-3.5 h-3.5" strokeWidth={2.5} />
                    ) : (
                      <Plus className="w-3.5 h-3.5" strokeWidth={2.5} />
                    )}
                  </button>
                ) : null}
              </div>
            ) : null}
          </div>
        ) : null}
        {subtitle ? <Snippet text={subtitle} compact={indented} /> : null}
      </div>
    </div>
  );
}

function atomTitle(result: SemanticSearchResult): string {
  const firstLine = result.content.split('\n')[0].trim().replace(/^#+\s*/, '');
  return firstLine || 'Untitled';
}

/**
 * Build a snippet string centered on a single match offset, with the match
 * wrapped in the PUA markers that `Snippet` recognizes for bolding. The
 * `offset` MUST already be in UTF-16 code units (the caller runs it through
 * `byteOffsetsToUtf16`); otherwise any non-ASCII character before the match
 * shifts the bold region to the wrong word.
 */
function buildMatchSnippet(content: string, offset: MatchOffset): string {
  const start = Math.max(0, offset.start - MATCH_SNIPPET_PAD);
  const end = Math.min(content.length, offset.end + MATCH_SNIPPET_PAD);
  const prefix = start > 0 ? '…' : '';
  const suffix = end < content.length ? '…' : '';
  return (
    prefix +
    content.slice(start, offset.start) +
    '\u{E000}' +
    content.slice(offset.start, offset.end) +
    '\u{E001}' +
    content.slice(offset.end, end) +
    suffix
  );
}

export function SearchPalette({ isOpen, onClose, initialQuery = '' }: SearchPaletteProps) {
  const overlayRef = useRef<HTMLDivElement>(null);
  const scrollContainerRef = useRef<HTMLDivElement>(null);
  const [usingKeyboard, setUsingKeyboard] = useState(false);
  const {
    setQuery,
    prefix,
    mode,
    searchQuery,
    selectedIndex,
    isSearching,
    searchError,
    canLoadMore,
    loadMore,
    visibleGlobalResults,
    searchSources,
    toggleSearchSource,
    hybridAtomResults,
    tagResults,
    expandedAtomIds,
    expandedWikiIds,
    toggleAtomExpanded,
    toggleWikiExpanded,
    handleKeyDown,
    handleSelect,
    canExpandSelected,
  } = useSearchPalette({ isOpen, onClose, initialQuery });

  useEffect(() => {
    if (isOpen) {
      document.body.style.overflow = 'hidden';
    }
    return () => {
      document.body.style.overflow = '';
    };
  }, [isOpen]);

  // Keep the currently-selected row in view as the user arrows through the
  // list. `block: 'nearest'` means we only scroll when the row is actually
  // off-screen. `behavior: 'instant'` guards against any ancestor with
  // `scroll-behavior: smooth` — a smooth animation desynchronises the scroll
  // from the selection paint and reads as jitter.
  useEffect(() => {
    if (!isOpen) return;
    const el = scrollContainerRef.current?.querySelector<HTMLElement>(
      '[data-palette-selected="true"]',
    );
    el?.scrollIntoView({ block: 'nearest', behavior: 'instant' as ScrollBehavior });
  }, [isOpen, selectedIndex]);

  // When the user is driving via keyboard, rows that slide past the mouse
  // cursor during scroll would otherwise flash :hover state — we suppress
  // the hover class until a real mousemove indicates they're back on mouse.
  const handleKeyDownWrapped = useCallback(
    (e: React.KeyboardEvent) => {
      if (!usingKeyboard) setUsingKeyboard(true);
      handleKeyDown(e);
    },
    [handleKeyDown, usingKeyboard],
  );
  const handleMouseMove = useCallback(() => {
    if (usingKeyboard) setUsingKeyboard(false);
  }, [usingKeyboard]);
  const handleScroll = useCallback((event: React.UIEvent<HTMLDivElement>) => {
    const element = event.currentTarget;
    if (canLoadMore && element.scrollHeight - element.scrollTop - element.clientHeight < 80) {
      void loadMore();
    }
  }, [canLoadMore, loadMore]);

  useEffect(() => {
    const element = scrollContainerRef.current;
    if (isOpen && canLoadMore && element && element.scrollHeight <= element.clientHeight) {
      void loadMore();
    }
  }, [canLoadMore, isOpen, loadMore, visibleGlobalResults]);

  if (!isOpen) return null;

  const handleOverlayClick = (e: React.MouseEvent) => {
    if (e.target === overlayRef.current) {
      onClose();
    }
  };

  const handleInputChange = (value: string) => {
    setQuery(prefix ? `${prefix.token}${value}` : value);
  };

  const handleClearPrefix = () => {
    setQuery(searchQuery);
  };

  let runningIndex = 0;

  const renderAtoms = (results: SemanticSearchResult[]) => {
    if (results.length === 0) return null;
    const rows: React.ReactNode[] = [];
    for (const result of results) {
      const offsets = result.match_offsets ?? [];
      const totalMatches = result.match_count ?? offsets.length;
      const expandable = totalMatches > 1;
      const expanded = expandable && expandedAtomIds.has(result.id);
      const headerIdx = runningIndex++;
      rows.push(
        <PaletteItem
          key={`atom-${result.id}`}
          selected={selectedIndex === headerIdx}
          onClick={() => handleSelect(headerIdx)}
          icon={<FileText className="w-4 h-4" strokeWidth={2} />}
          title={atomTitle(result)}
          // When expanded, the per-match sub-rows carry the snippets; showing
          // the header snippet too would just be visual noise.
          subtitle={expanded ? undefined : result.match_snippet ?? result.matching_chunk_content}
          meta={
            result.match_offsets
              ? `${totalMatches} ${totalMatches === 1 ? 'match' : 'matches'}`
              : `${Math.round(result.similarity_score * 100)}%`
          }
          disclosure={expandable ? (expanded ? 'expanded' : 'collapsed') : undefined}
          onDisclosureClick={expandable ? () => toggleAtomExpanded(result.id) : undefined}
        />,
      );
      if (expanded) {
        // Convert byte offsets to UTF-16 indices so string slicing lines up
        // with the bold region the user sees, regardless of non-ASCII content.
        const utf16Offsets = byteOffsetsToUtf16(result.content, offsets);
        for (let mIdx = 0; mIdx < utf16Offsets.length; mIdx++) {
          const offset = utf16Offsets[mIdx];
          const subIdx = runningIndex++;
          rows.push(
            <PaletteItem
              key={`atom-${result.id}-match-${mIdx}`}
              selected={selectedIndex === subIdx}
              onClick={() => handleSelect(subIdx)}
              icon={null}
              indented
              subtitle={buildMatchSnippet(result.content, offset)}
            />,
          );
        }
        const hiddenCount = totalMatches - offsets.length;
        if (hiddenCount > 0) {
          const moreIdx = runningIndex++;
          rows.push(
            <PaletteItem
              key={`atom-${result.id}-more`}
              selected={selectedIndex === moreIdx}
              onClick={() => handleSelect(moreIdx)}
              icon={null}
              indented
              title={`+${hiddenCount} more ${hiddenCount === 1 ? 'match' : 'matches'}`}
            />,
          );
        }
      }
    }
    return (
      <div className="mb-2">
        <SectionHeader label="Atoms" count={results.length} />
        {rows}
      </div>
    );
  };

  const renderWiki = (results: GlobalWikiSearchResult[]) => {
    if (results.length === 0) return null;
    const rows: React.ReactNode[] = [];
    for (const result of results) {
      const offsets = result.match_offsets ?? [];
      const totalMatches = result.match_count ?? offsets.length;
      const expandable = totalMatches > 1;
      const expanded = expandable && expandedWikiIds.has(result.id);
      const headerIdx = runningIndex++;
      rows.push(
        <PaletteItem
          key={`wiki-${result.id}`}
          selected={selectedIndex === headerIdx}
          onClick={() => handleSelect(headerIdx)}
          icon={<BookOpen className="w-4 h-4" strokeWidth={2} />}
          title={result.tag_name}
          subtitle={
            expanded
              ? undefined
              : result.match_snippet ?? result.content_snippet
          }
          meta={
            result.match_offsets
              ? `${totalMatches} ${totalMatches === 1 ? 'match' : 'matches'}`
              : `${result.atom_count} atoms`
          }
          disclosure={expandable ? (expanded ? 'expanded' : 'collapsed') : undefined}
          onDisclosureClick={expandable ? () => toggleWikiExpanded(result.id) : undefined}
        />,
      );
      if (expanded) {
        const utf16Offsets = byteOffsetsToUtf16(result.content, offsets);
        for (let mIdx = 0; mIdx < utf16Offsets.length; mIdx++) {
          const offset = utf16Offsets[mIdx];
          const subIdx = runningIndex++;
          rows.push(
            <PaletteItem
              key={`wiki-${result.id}-match-${mIdx}`}
              selected={selectedIndex === subIdx}
              onClick={() => handleSelect(subIdx)}
              icon={null}
              indented
              subtitle={buildMatchSnippet(result.content, offset)}
            />,
          );
        }
        const hiddenCount = totalMatches - offsets.length;
        if (hiddenCount > 0) {
          const moreIdx = runningIndex++;
          rows.push(
            <PaletteItem
              key={`wiki-${result.id}-more`}
              selected={selectedIndex === moreIdx}
              onClick={() => handleSelect(moreIdx)}
              icon={null}
              indented
              title={`+${hiddenCount} more ${hiddenCount === 1 ? 'match' : 'matches'}`}
            />,
          );
        }
      }
    }
    return (
      <div className="mb-2">
        <SectionHeader label="Wiki" count={results.length} />
        {rows}
      </div>
    );
  };

  const renderChats = (results: GlobalChatSearchResult[]) => {
    if (results.length === 0) return null;
    const start = runningIndex;
    runningIndex += results.length;
    return (
      <div className="mb-2">
        <SectionHeader label="Chats" count={results.length} />
        {results.map((result, idx) => (
          <PaletteItem
            key={`chat-${result.id}`}
            selected={selectedIndex === start + idx}
            onClick={() => handleSelect(start + idx)}
            icon={<MessageCircle className="w-4 h-4" strokeWidth={2} />}
            title={result.title || 'Untitled conversation'}
            subtitle={result.matching_message_content}
            meta={`${result.message_count} messages`}
          />
        ))}
      </div>
    );
  };

  const renderTags = (results: GlobalTagSearchResult[]) => {
    if (results.length === 0) return null;
    const start = runningIndex;
    runningIndex += results.length;
    return (
      <div className="mb-2">
        <SectionHeader label="Tags" count={results.length} />
        {results.map((result, idx) => (
          <PaletteItem
            key={`tag-${result.id}`}
            selected={selectedIndex === start + idx}
            onClick={() => handleSelect(start + idx)}
            icon={<Hash className="w-4 h-4" strokeWidth={2} />}
            title={result.name}
            meta={`${result.atom_count} atoms`}
          />
        ))}
      </div>
    );
  };

  const showEmptyState =
    !isSearching &&
    !searchError &&
    searchQuery.trim().length >= 2 &&
    ((mode === 'tags' && tagResults.length === 0) ||
      (mode === 'atoms-hybrid' && hybridAtomResults.length === 0) ||
      (mode === 'global' &&
        visibleGlobalResults.atoms.length === 0 &&
        visibleGlobalResults.wiki.length === 0 &&
        visibleGlobalResults.chats.length === 0 &&
        visibleGlobalResults.tags.length === 0));

  return createPortal(
    <KeyboardNavContext.Provider value={usingKeyboard}>
    <div
      ref={overlayRef}
      onClick={handleOverlayClick}
      data-modal="true"
      className="fixed inset-0 z-50 flex items-start justify-center pt-[15vh] bg-black/50 backdrop-blur-sm safe-area-padding"
    >
      <div
        onKeyDown={handleKeyDownWrapped}
        className="w-full max-w-2xl mx-4 bg-[var(--color-bg-panel)] rounded-xl shadow-2xl border border-[var(--color-border)] animate-in fade-in zoom-in-95 duration-200 overflow-hidden"
      >
        {mode === 'global' ? (
          <div className="flex flex-wrap gap-x-4 gap-y-1 px-4 pt-3 text-xs text-[var(--color-text-secondary)]">
            {(['atoms', 'wiki', 'chats', 'tags'] as const).map((source) => (
              <label key={source} className="flex cursor-pointer items-center gap-1.5 capitalize">
                <input
                  type="checkbox"
                  checked={searchSources[source]}
                  onChange={() => toggleSearchSource(source)}
                  className="accent-[var(--color-accent)]"
                />
                {source}
              </label>
            ))}
          </div>
        ) : null}
        <CommandInput
          query={searchQuery}
          onChange={handleInputChange}
          isSearching={isSearching}
          prefix={prefix}
          onClearPrefix={handleClearPrefix}
          shortcutHint={null}
          placeholder={
            mode === 'tags'
              ? 'Search tags...'
              : mode === 'atoms-hybrid'
                ? 'Search atoms semantically...'
                : 'Search atoms, wiki, chats, and tags...'
          }
        />

        <div
          ref={scrollContainerRef}
          onMouseMove={handleMouseMove}
          onScroll={handleScroll}
          className="overflow-y-auto max-h-[50vh] py-2"
        >
          {!searchQuery.trim() ? (
            <div className="px-4 py-8 text-center text-[var(--color-text-tertiary)] text-sm">
              Start typing to search across Atomic. Use `#` for tags or `&gt;` for semantic atom search.
            </div>
          ) : searchQuery.trim().length < 2 && mode !== 'tags' ? (
            <div className="px-4 py-8 text-center text-[var(--color-text-tertiary)] text-sm">
              Type at least 2 characters to search.
            </div>
          ) : null}

          {mode === 'global' && searchQuery.trim().length >= 2 ? (
            <>
              {renderAtoms(visibleGlobalResults.atoms)}
              {renderWiki(visibleGlobalResults.wiki)}
              {renderChats(visibleGlobalResults.chats)}
              {renderTags(visibleGlobalResults.tags)}
            </>
          ) : null}

          {mode === 'atoms-hybrid' && searchQuery.trim().length >= 2 ? renderAtoms(hybridAtomResults) : null}

          {mode === 'tags' && searchQuery.trim().length >= 2 ? renderTags(tagResults) : null}

          {searchError ? (
            <div className="mx-4 my-3 rounded border border-red-500/40 bg-red-500/10 p-3 text-sm text-red-500">
              Search failed: {searchError}
            </div>
          ) : null}

          {showEmptyState ? (
            <div className="px-4 py-8 text-center text-[var(--color-text-tertiary)] text-sm">
              No matches found for "{searchQuery}".
            </div>
          ) : null}
        </div>

        <div className="px-4 py-2 border-t border-[var(--color-border)] flex items-center justify-between text-[11px] text-[var(--color-text-primary)]">
          <div className="flex items-center gap-3">
            <span className="flex items-center gap-1">
              <kbd className="px-1 py-0.5 bg-[var(--color-bg-hover)] border border-[var(--color-border-hover)] rounded text-[var(--color-text-primary)]">↑↓</kbd>
              navigate
            </span>
            <span className="flex items-center gap-1">
              <kbd className="px-1 py-0.5 bg-[var(--color-bg-hover)] border border-[var(--color-border-hover)] rounded text-[var(--color-text-primary)]">↵</kbd>
              open
            </span>
            {canExpandSelected ? (
              <span className="flex items-center gap-1">
                <kbd className="px-1 py-0.5 bg-[var(--color-bg-hover)] border border-[var(--color-border-hover)] rounded text-[var(--color-text-primary)]">→</kbd>
                expand matches
              </span>
            ) : null}
            <span className="flex items-center gap-1">
              <kbd className="px-1 py-0.5 bg-[var(--color-bg-hover)] border border-[var(--color-border-hover)] rounded text-[var(--color-text-primary)]">esc</kbd>
              close
            </span>
          </div>
          <div className="flex items-center gap-3">
            <span className="flex items-center gap-1">
              <kbd className="px-1 py-0.5 bg-[var(--color-bg-hover)] border border-[var(--color-border-hover)] rounded text-[var(--color-text-primary)]">#</kbd>
              tags only
            </span>
            <span className="flex items-center gap-1">
              <kbd className="px-1 py-0.5 bg-[var(--color-bg-hover)] border border-[var(--color-border-hover)] rounded text-[var(--color-text-primary)]">&gt;</kbd>
              semantic atoms
            </span>
          </div>
        </div>
      </div>
    </div>
    </KeyboardNavContext.Provider>,
    document.body
  );
}
