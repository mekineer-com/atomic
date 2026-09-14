import { useMemo, useCallback, useEffect, useRef, useState, type ReactNode } from 'react';
import { useShallow } from 'zustand/react/shallow';
import {
  PanelLeft,
  MessageCircle,
  LayoutDashboard,
  LayoutGrid,
  List as ListIcon,
  Library,
  Network,
  BookOpen,
  Search,
  Filter,
  Telescope,
  ClipboardCheck,
  Users,
} from 'lucide-react';
import { motion, LayoutGroup } from 'motion/react';
import { AtomGrid } from '../atoms/AtomGrid';
import { AtomList } from '../atoms/AtomList';
import { AtomReader } from '../atoms/AtomReader';
import { FilterBar } from '../atoms/FilterBar';
import { FilterSheet } from '../atoms/FilterSheet';
import { SigmaCanvas } from '../canvas/SigmaCanvas';
import { LocalGraphView } from '../canvas/LocalGraphView';
import { DashboardView } from '../dashboard/DashboardView';
import { FAB } from '../ui/FAB';
import { WikiFullView } from '../wiki/WikiFullView';
import { WikiReader } from '../wiki/WikiReader';
import { ReportsFullView, ReportDetailView, FindingReader } from '../reports';
import { ChatViewer } from '../chat/ChatViewer';
import { PendingReviewPanel } from '../memu/PendingReviewPanel';
import { EntityManager, EntityReader } from '../memu/EntityManager';
import { TabStrip } from './TabStrip';
import { useAtomsStore } from '../../stores/atoms';
import { useTagsStore } from '../../stores/tags';
import { useCanvasStore } from '../../stores/canvas';
import { useUIStore } from '../../stores/ui';
import { isTauri } from '../../lib/platform';
import { getTransport } from '../../lib/transport';
import { useIsMobile } from '../../hooks';
import { startNewAtom } from '../../lib/new-atom';
import { readerEditorActions } from '../../lib/reader-editor-bridge';

export function MainView({ soulSelector }: { soulSelector?: ReactNode }) {
  const atoms = useAtomsStore(s => s.atoms);
  const totalCount = useAtomsStore(s => s.totalCount);
  const hasMore = useAtomsStore(s => s.hasMore);
  const isLoadingInitial = useAtomsStore(s => s.isLoadingInitial);
  const isLoadingMore = useAtomsStore(s => s.isLoadingMore);
  const fetchNextPage = useAtomsStore(s => s.fetchNextPage);
  const semanticSearchResults = useAtomsStore(s => s.semanticSearchResults);
  const semanticSearchQuery = useAtomsStore(s => s.semanticSearchQuery);
  const retryEmbedding = useAtomsStore(s => s.retryEmbedding);
  const retryTagging = useAtomsStore(s => s.retryTagging);
  const sourceFilter = useAtomsStore(s => s.sourceFilter);
  const sourceValue = useAtomsStore(s => s.sourceValue);
  const sortBy = useAtomsStore(s => s.sortBy);
  const sortOrder = useAtomsStore(s => s.sortOrder);
  const search = useAtomsStore(s => s.search);
  const clearSemanticSearch = useAtomsStore(s => s.clearSemanticSearch);
  const fetchAtoms = useAtomsStore(s => s.fetchAtoms);
  const createAtom = useAtomsStore(s => s.createAtom);

  const { viewMode, atomsLayout, knowledgeSource, searchQuery, activeTabId, tabs } = useUIStore(
    useShallow(s => ({
      viewMode: s.viewMode,
      atomsLayout: s.atomsLayout,
      knowledgeSource: s.knowledgeSource,
      searchQuery: s.searchQuery,
      activeTabId: s.activeTabId,
      tabs: s.tabs,
    }))
  );
  const leftPanelOpen = useUIStore(s => s.leftPanelOpen);
  const setLeftPanelOpen = useUIStore(s => s.setLeftPanelOpen);
  const toggleLeftPanel = useUIStore(s => s.toggleLeftPanel);
  const deactivateTabs = useUIStore(s => s.deactivateTabs);
  const openToolTab = useUIStore(s => s.openToolTab);
  const setViewMode = useUIStore(s => s.setViewMode);
  const setAtomsLayout = useUIStore(s => s.setAtomsLayout);
  const setKnowledgeSource = useUIStore(s => s.setKnowledgeSource);
  const openReader = useUIStore(s => s.openReader);
  const openReaderEditing = useUIStore(s => s.openReaderEditing);
  const readerState = useUIStore(s => s.readerState);
  const wikiReaderState = useUIStore(s => s.wikiReaderState);
  const reportsDetailState = useUIStore(s => s.reportsDetailState);
  const findingReaderState = useUIStore(s => s.findingReaderState);
  const localGraph = useUIStore(s => s.localGraph);

  const openSearchPalette = useUIStore(s => s.openSearchPalette);

  const chatSidebarOpen = useUIStore(s => s.chatSidebarOpen);
  const chatSidebarWidth = useUIStore(s => s.chatSidebarWidth);
  const setChatSidebarWidth = useUIStore(s => s.setChatSidebarWidth);
  const toggleChatSidebar = useUIStore(s => s.toggleChatSidebar);
  const [isResizingChat, setIsResizingChat] = useState(false);

  const [filterBarOpen, setFilterBarOpen] = useState(false);
  const [memuReviewsEnabled, setMemuReviewsEnabled] = useState(false);
  const activeTab = tabs.find(tab => tab.id === activeTabId);
  const activeEntry = activeTab?.stack[activeTab.stackIndex];
  const reviewPanelOpen = activeEntry?.type === 'tool' && activeEntry.tool === 'approvals';
  const entityPanelOpen = activeEntry?.type === 'tool' && activeEntry.tool === 'entities';
  const reviewPanelMounted = tabs.some(tab => tab.stack.some(entry => entry.type === 'tool' && entry.tool === 'approvals'));
  const entityPanelMounted = tabs.some(tab => tab.stack.some(entry => entry.type === 'tool' && entry.tool === 'entities'));
  const isMobile = useIsMobile();
  const hasActiveFilter = sourceFilter !== 'all' || !!sourceValue || sortBy !== 'updated' || sortOrder !== 'desc';

  // Main nav is "active" only when no tab is open and the current view mode
  // matches. Once a tab is active, the pill carries the active styling and
  // the main nav goes back to a neutral state.
  const onBaseView = activeTabId === null;

  useEffect(() => {
    let ignore = false;
    (async () => {
      try {
        const status = await getTransport().invoke<{ enabled: boolean }>('get_memu_review_status');
        if (!ignore) setMemuReviewsEnabled(status.enabled);
      } catch {
        if (!ignore) setMemuReviewsEnabled(false);
      }
    })();
    return () => {
      ignore = true;
    };
  }, []);

  // Debounced server-side search when searchQuery changes
  const searchTimerRef = useRef<ReturnType<typeof setTimeout>>();
  useEffect(() => {
    if (searchTimerRef.current) clearTimeout(searchTimerRef.current);

    const query = searchQuery.trim();
    if (!query) {
      // Clear search results when query is empty
      if (semanticSearchResults !== null) {
        clearSemanticSearch();
      }
      return;
    }

    // Debounce 300ms before triggering API search
    searchTimerRef.current = setTimeout(() => {
      search(query);
    }, 300);

    return () => {
      if (searchTimerRef.current) clearTimeout(searchTimerRef.current);
    };
  }, [searchQuery]);

  // Determine what to display
  const displayAtoms = useMemo(() => {
    // If semantic search is active, use those results
    if (semanticSearchResults !== null) {
      return semanticSearchResults;
    }
    return atoms;
  }, [atoms, semanticSearchResults]);

  // Check if we're showing semantic search results
  const isSemanticSearch = semanticSearchResults !== null;

  // Build lookup map for matching chunk content (avoids .find() per atom)
  const matchingChunkMap = useMemo(() => {
    if (!isSemanticSearch) return null;
    const map = new Map<string, string>();
    for (const r of semanticSearchResults) {
      if (r.matching_chunk_content) {
        map.set(r.id, r.matching_chunk_content);
      }
    }
    return map;
  }, [isSemanticSearch, semanticSearchResults]);

  const getMatchingChunkContent = useCallback((atomId: string): string | undefined => {
    return matchingChunkMap?.get(atomId);
  }, [matchingChunkMap]);

  const handleAtomClick = useCallback((atomId: string, opts?: { newTab?: boolean }) => {
    // Pass highlight text based on search mode:
    // - Keyword: highlight the search query terms
    // - Semantic: highlight the matching chunk content
    // - Hybrid: highlight the search query (prioritize keywords over chunk)
    const isSearch = useAtomsStore.getState().semanticSearchResults !== null;
    if (!isSearch) {
      openReader(atomId, undefined, opts);
      return;
    }
    const mode = useAtomsStore.getState().searchMode;
    const query = useAtomsStore.getState().semanticSearchQuery;
    let highlightText: string | undefined;
    if (mode === 'keyword' || mode === 'hybrid') {
      highlightText = query;
    } else {
      highlightText = matchingChunkMap?.get(atomId);
    }
    openReader(atomId, highlightText, opts);
  }, [openReader, matchingChunkMap]);

  const handleNewAtom = useCallback(async () => {
    try {
      if (knowledgeSource === 'workspace') {
        const atom = await createAtom('');
        openReaderEditing(atom.id);
        return;
      }
      await startNewAtom();
    } catch (error) {
      console.error('Failed to create atom:', error);
    }
  }, [createAtom, knowledgeSource, openReaderEditing]);

  const handleKnowledgeSource = useCallback(async (source: 'memories' | 'workspace') => {
    if (source === knowledgeSource) return;
    deactivateTabs();
    setKnowledgeSource(source);
    getTransport().setWorkspaceMode(source === 'workspace');
    if (source === 'workspace' && (viewMode === 'dashboard' || viewMode === 'reports')) {
      setViewMode('atoms');
    }
    useAtomsStore.getState().reset();
    useTagsStore.getState().reset();
    useUIStore.getState().resetCanvasLayerState();
    useCanvasStore.getState().invalidateCanvasData();
    await Promise.all([fetchAtoms(), useTagsStore.getState().fetchTags()]);
  }, [deactivateTabs, fetchAtoms, knowledgeSource, setKnowledgeSource, setViewMode, viewMode]);

  const handleRetryEmbedding = useCallback(async (atomId: string) => {
    try {
      await retryEmbedding(atomId);
    } catch (error) {
      console.error('Failed to retry embedding:', error);
    }
  }, [retryEmbedding]);

  const handleRetryTagging = useCallback(async (atomId: string) => {
    try {
      await retryTagging(atomId);
    } catch (error) {
      console.error('Failed to retry tagging:', error);
    }
  }, [retryTagging]);

  const handleOpenChat = useCallback(() => {
    toggleChatSidebar();
  }, [toggleChatSidebar]);

  const handleChatResizeStart = useCallback((e: React.MouseEvent) => {
    e.preventDefault();
    const startX = e.clientX;
    const startWidth = useUIStore.getState().chatSidebarWidth;
    setIsResizingChat(true);

    const onMouseMove = (e: MouseEvent) => {
      const delta = startX - e.clientX;
      setChatSidebarWidth(startWidth + delta);
    };

    const onMouseUp = () => {
      document.removeEventListener('mousemove', onMouseMove);
      document.removeEventListener('mouseup', onMouseUp);
      document.body.style.cursor = '';
      document.body.style.userSelect = '';
      setIsResizingChat(false);
    };

    document.addEventListener('mousemove', onMouseMove);
    document.addEventListener('mouseup', onMouseUp);
    document.body.style.cursor = 'col-resize';
    document.body.style.userSelect = 'none';
  }, [setChatSidebarWidth]);

  const handleOpenSearch = useCallback(() => {
    const atomId = readerState.atomId;
    if (atomId && !atomId.startsWith('memory:') && !atomId.startsWith('category:') && !atomId.startsWith('entity:')) {
      readerEditorActions.current?.openSearch(readerState.highlightText ?? undefined);
      return;
    }
    openSearchPalette();
  }, [openSearchPalette, readerState.atomId, readerState.highlightText]);

  const handleLoadMore = useCallback(() => {
    if (!isSemanticSearch && hasMore) {
      fetchNextPage();
    }
  }, [isSemanticSearch, hasMore, fetchNextPage]);

  // Display count: totalCount from server when not searching, results length when searching
  const displayCount = isSemanticSearch ? displayAtoms.length : totalCount;

  return (
    <>
    <main className="relative flex-1 flex flex-col h-full bg-[var(--color-bg-main)] overflow-hidden">
      {/* Titlebar row — the row itself is a Tauri drag region; interactive
          elements inside it (buttons, tabs) receive their own events normally. */}
      <div
        data-tauri-drag-region
        className={`h-[52px] flex items-center gap-3 px-4 flex-shrink-0 drag-region ${!leftPanelOpen && isTauri() ? 'pl-[78px]' : ''}`}
      >
        {/* Left sidebar toggle — always visible */}
        <button
          onClick={toggleLeftPanel}
          className={`p-1.5 rounded-md transition-colors ${
            leftPanelOpen
              ? 'text-[var(--color-text-primary)] hover:bg-[var(--color-bg-hover)]'
              : 'text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] hover:bg-[var(--color-bg-hover)]'
          }`}
          title={leftPanelOpen ? "Hide sidebar" : "Show sidebar"}
        >
          <PanelLeft className="w-4 h-4" strokeWidth={2} />
        </button>

        {/* Main nav + tabs share a single LayoutGroup so the accent blob
            (Motion `layoutId="active-tab-blob"`) can FLIP between any
            main-nav button and any tab pill, giving the active highlight
            a continuous slide instead of a fade-swap when the user
            changes views. */}
        <LayoutGroup>
          {!isMobile && (
            <div className="flex items-center gap-1 shrink-0">
              {([
                ['dashboard', LayoutDashboard, 'Dashboard'],
                ['atoms', Library, 'Atoms'],
                ['canvas', Network, 'Canvas view'],
                ['wiki', BookOpen, 'Wiki view'],
                ['reports', Telescope, 'Reports'],
              ] as const).filter(([mode]) => knowledgeSource === 'memories' || !['dashboard', 'reports'].includes(mode)).map(([mode, IconCmp, label]) => {
                const isActiveNav = onBaseView && viewMode === mode;
                return (
                  <button
                    key={mode}
                    onClick={() => {
                      setViewMode(mode);
                    }}
                    className={`relative p-1.5 rounded-md ${
                      isActiveNav
                        ? 'text-white'
                        : 'text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] hover:bg-[var(--color-bg-hover)] transition-colors'
                    }`}
                    title={label}
                    aria-label={label}
                  >
                    {isActiveNav && (
                      <motion.div
                        layoutId="active-tab-blob"
                        className="absolute inset-0 bg-[var(--color-accent)] rounded-md shadow-sm"
                        transition={{ type: 'spring', stiffness: 520, damping: 32, mass: 0.9 }}
                      />
                    )}
                    <IconCmp className="relative z-[1] w-4 h-4" strokeWidth={2} />
                  </button>
                );
              })}
            </div>
          )}

          {/* memU readers have no inline editor, so their search stays global. */}
          <button
            onClick={handleOpenSearch}
            className="p-1.5 rounded-md text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] hover:bg-[var(--color-bg-hover)] transition-colors shrink-0"
            title={readerState.atomId ? 'Find in note' : 'Search atoms'}
          >
            <Search className="w-4 h-4" strokeWidth={2} />
          </button>

          {/* Tab strip — pills sit immediately to the right of search and
              consume any unused horizontal space. */}
          <div className="flex-1 min-w-0 flex items-center">
            <TabStrip />
          </div>
        </LayoutGroup>

        {/* Save status — visible whenever an atom tab is active and saving */}
        {readerState.atomId && readerState.saveStatus !== 'idle' && (
          <span className={`text-xs shrink-0 ${
            readerState.saveStatus === 'saving' ? 'text-[var(--color-text-tertiary)]' :
            readerState.saveStatus === 'saved' ? 'text-green-500' :
            'text-red-500'
          }`}>
            {readerState.saveStatus === 'saving' ? 'Saving...' :
             readerState.saveStatus === 'saved' ? 'Saved' : 'Save failed'}
          </span>
        )}

        {/* Filter toggle + atom count — base view + atoms only. */}
        {!reviewPanelOpen && !entityPanelOpen && onBaseView && (isMobile || viewMode === 'atoms') && (
          <div className="flex items-center gap-2 shrink-0">
            <button
              onClick={() => setFilterBarOpen(!filterBarOpen)}
              className={`relative p-1.5 rounded-md transition-colors ${
                filterBarOpen || hasActiveFilter
                  ? 'text-[var(--color-accent-light)] hover:text-[var(--color-accent)]'
                  : 'text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] hover:bg-[var(--color-bg-hover)]'
              }`}
              title="Filter & sort"
            >
              <Filter className="w-4 h-4" strokeWidth={2} />
              {hasActiveFilter && !filterBarOpen && (
                <span className="absolute top-0.5 right-0.5 w-1.5 h-1.5 bg-[var(--color-accent)] rounded-full" />
              )}
            </button>
            {!isMobile && (
              <span className="text-sm text-[var(--color-text-secondary)]">
                {displayCount} atom{displayCount !== 1 ? 's' : ''}
              </span>
            )}
          </div>
        )}

        {/* Atoms layout sub-toggle — sits right-aligned next to the chat
            button so the cluster of left-side nav stays stable when
            switching views. Desktop atoms-base-view only. */}
        {!reviewPanelOpen && !entityPanelOpen && !isMobile && onBaseView && viewMode === 'atoms' && (
          <div className="flex items-center bg-[var(--color-bg-card)] rounded-md border border-[var(--color-border)] shrink-0">
            <button
              onClick={() => setAtomsLayout('grid')}
              className={`p-1.5 rounded-l-md transition-colors ${
                atomsLayout === 'grid'
                  ? 'text-[var(--color-text-primary)] bg-[var(--color-bg-hover)]'
                  : 'text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)]'
              }`}
              title="Grid layout"
            >
              <LayoutGrid className="w-4 h-4" strokeWidth={2} />
            </button>
            <button
              onClick={() => setAtomsLayout('list')}
              className={`p-1.5 rounded-r-md transition-colors ${
                atomsLayout === 'list'
                  ? 'text-[var(--color-text-primary)] bg-[var(--color-bg-hover)]'
                  : 'text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)]'
              }`}
              title="List layout"
            >
              <ListIcon className="w-4 h-4" strokeWidth={2} />
            </button>
          </div>
        )}

        {memuReviewsEnabled && (
          <div className="flex rounded-md border border-[var(--color-border)] p-0.5 text-xs">
            {(['memories', 'workspace'] as const).map((source) => (
              <button
                key={source}
                type="button"
                onClick={() => void handleKnowledgeSource(source)}
                className={`rounded px-2 py-1 capitalize ${knowledgeSource === source ? 'bg-[var(--color-accent)] text-white' : 'text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)]'}`}
              >
                {source}
              </button>
            ))}
          </div>
        )}

        {soulSelector}

        {memuReviewsEnabled && (
          <button
            onClick={() => {
              setLeftPanelOpen(false);
              openToolTab('approvals');
            }}
            className="p-1.5 rounded-md text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] hover:bg-[var(--color-bg-hover)] transition-colors shrink-0"
            title="Review memU changes"
          >
            <ClipboardCheck className="w-4 h-4" strokeWidth={2} />
          </button>
        )}

        {memuReviewsEnabled && (
          <button
            onClick={() => {
              setLeftPanelOpen(false);
              openToolTab('entities');
            }}
            className={`p-1.5 rounded-md transition-colors shrink-0 ${entityPanelOpen ? 'text-white bg-[var(--color-accent)]' : 'text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] hover:bg-[var(--color-bg-hover)]'}`}
            title="Entities"
          >
            <Users className="w-4 h-4" strokeWidth={2} />
          </button>
        )}

        {/* Chat sidebar toggle */}
        <button
          onClick={handleOpenChat}
          className={`p-1.5 rounded-md transition-colors shrink-0 ${
            chatSidebarOpen
              ? 'text-[var(--color-text-primary)] hover:bg-[var(--color-bg-hover)]'
              : 'text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] hover:bg-[var(--color-bg-hover)]'
          }`}
          title={chatSidebarOpen ? "Hide chat" : "Show chat"}
        >
          <MessageCircle className="w-4 h-4" strokeWidth={2} />
        </button>
      </div>

      {/* Search results header - only show in atoms view */}
      {!reviewPanelOpen && !entityPanelOpen && isSemanticSearch && viewMode === 'atoms' && (
        <div className="px-4 py-2 text-sm text-[var(--color-text-secondary)] border-b border-[var(--color-border)]">
          {semanticSearchResults.length > 0 ? (
            <span>
              {semanticSearchResults.length} results for "{semanticSearchQuery}"
            </span>
          ) : (
            <span>No atoms match your search</span>
          )}
        </div>
      )}

      {/* Filter bar — desktop inline strip, atoms view only */}
      {!reviewPanelOpen && !entityPanelOpen && !isMobile && !isSemanticSearch && viewMode === 'atoms' && filterBarOpen && <FilterBar />}

      {/* Filter sheet — mobile bottom sheet hosts view mode + filter + sort */}
      {!reviewPanelOpen && !entityPanelOpen && isMobile && (
        <FilterSheet
          isOpen={filterBarOpen}
          onClose={() => setFilterBarOpen(false)}
          displayCount={displayCount}
        />
      )}

      {/* Content */}
      <div className="flex-1 overflow-hidden relative">
        {reviewPanelMounted && (
          <div className={`absolute inset-0 ${reviewPanelOpen ? '' : 'hidden'}`}><PendingReviewPanel /></div>
        )}
        {entityPanelMounted && (
          <div className={`absolute inset-0 ${entityPanelOpen ? '' : 'hidden'}`}><EntityManager /></div>
        )}
        {!reviewPanelOpen && !entityPanelOpen && (localGraph.isOpen && localGraph.centerAtomId ? (
          <LocalGraphView />
        ) : readerState.atomId ? (
          readerState.atomId.startsWith('entity:')
            ? <EntityReader entityId={readerState.atomId.slice('entity:'.length)} />
            : <AtomReader atomId={readerState.atomId} highlightText={readerState.highlightText} initialEditing={readerState.editing} />
        ) : wikiReaderState.tagId && wikiReaderState.tagName ? (
          <WikiReader
            tagId={wikiReaderState.tagId}
            tagName={wikiReaderState.tagName}
            highlightText={wikiReaderState.highlightText}
          />
        ) : findingReaderState.atomId ? (
          <FindingReader atomId={findingReaderState.atomId} />
        ) : reportsDetailState.reportId ? (
          <ReportDetailView reportId={reportsDetailState.reportId} />
        ) : viewMode === 'dashboard' ? (
          <DashboardView />
        ) : viewMode === 'wiki' ? (
          <WikiFullView />
        ) : viewMode === 'reports' ? (
          <ReportsFullView />
        ) : viewMode === 'canvas' ? (
          <SigmaCanvas key={knowledgeSource} />
        ) : atomsLayout === 'grid' ? (
          <AtomGrid
            atoms={displayAtoms}
            onAtomClick={handleAtomClick}
            getMatchingChunkContent={isSemanticSearch ? getMatchingChunkContent : undefined}
            onRetryEmbedding={handleRetryEmbedding}
            onRetryTagging={handleRetryTagging}
            onLoadMore={handleLoadMore}
            isLoading={isLoadingInitial}
            isLoadingMore={isLoadingMore}
          />
        ) : (
          <AtomList
            atoms={displayAtoms}
            onAtomClick={handleAtomClick}
            getMatchingChunkContent={isSemanticSearch ? getMatchingChunkContent : undefined}
            onRetryEmbedding={handleRetryEmbedding}
            onRetryTagging={handleRetryTagging}
            onLoadMore={handleLoadMore}
            isLoading={isLoadingInitial}
            isLoadingMore={isLoadingMore}
          />
        ))}
      </div>

      {/* FAB — on atoms + dashboard base views only (no active tab) */}
      {!reviewPanelOpen && !entityPanelOpen && onBaseView && (viewMode === 'atoms' || viewMode === 'dashboard') && <FAB onClick={handleNewAtom} title={knowledgeSource === 'workspace' ? 'Create new note' : 'Create new memory'} />}
    </main>

    {/* Chat sidebar backdrop — mobile only */}
    <div
      className={`fixed inset-0 bg-black/40 z-30 md:hidden transition-opacity duration-200 ${
        chatSidebarOpen ? 'opacity-100' : 'opacity-0 pointer-events-none'
      }`}
      onClick={() => chatSidebarOpen && toggleChatSidebar()}
    />

    {/* Chat sidebar — available in all views.
        Desktop: flex sibling that animates width.
        Mobile: fixed overlay that slides in from the right. */}
    <div
      className={`
        relative flex-shrink-0 border-l border-[var(--color-border)] bg-[var(--color-bg-panel)] overflow-hidden
        max-md:fixed max-md:top-0 max-md:right-0 max-md:h-full max-md:w-full max-md:z-40 max-md:shadow-2xl
        max-md:pt-[env(safe-area-inset-top)] max-md:pb-[env(safe-area-inset-bottom)] max-md:pr-[env(safe-area-inset-right)]
        md:w-[var(--chat-w)]
        ${isResizingChat ? '' : 'transition-[width,transform] duration-300 ease-in-out'}
        ${chatSidebarOpen ? 'max-md:translate-x-0' : 'max-md:translate-x-full'}
        ${chatSidebarOpen ? '' : 'md:!w-0 md:border-l-0'}
      `}
      style={{ '--chat-w': `${chatSidebarWidth}px` } as React.CSSProperties}
    >
      {/* Resize handle — desktop only */}
      <div
        className="hidden md:block absolute left-0 top-0 h-full w-1.5 cursor-col-resize z-10 hover:bg-[var(--color-accent)]/20 active:bg-[var(--color-accent)]/30"
        onMouseDown={handleChatResizeStart}
      />
      <div className="w-full md:min-w-[var(--chat-w)] h-full">
        <ChatViewer />
      </div>
    </div>
    </>
  );
}
