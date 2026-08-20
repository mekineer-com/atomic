import { lazy, Suspense, useState, useEffect, useCallback, useMemo, useRef, type FormEvent } from 'react';
import { ChevronDown, Trash2 } from 'lucide-react';
import { openExternalUrl } from '../../lib/platform';
import { Modal } from '../ui/Modal';
import { Input } from '../ui/Input';
import { TagChip } from '../tags/TagChip';
import { TagSelector } from '../tags/TagSelector';
import { MiniGraphPreview } from '../canvas/MiniGraphPreview';
import { useAtomsStore, type AtomWithTags, type SemanticSearchResult, type SimilarAtomResult } from '../../stores/atoms';
import { useTagsStore } from '../../stores/tags';
import { useUIStore } from '../../stores/ui';
import { useCanvasStore } from '../../stores/canvas';
import { useInlineEditor } from '../../hooks';
import { formatDate } from '../../lib/date';
import { getTransport } from '../../lib/transport';
import { findSimilarAtoms } from '../../lib/api';
import { readerEditorActions } from '../../lib/reader-editor-bridge';
import { DossierMarkdown, MemoryCitationLinks } from '../memu/DossierMarkdown';
import { MemoryEntityControls } from '../memu/EntityManager';
import { atomLinkExtension, type AtomLinkSuggestion, type AtomLinkSuggestionSource } from '../../editor/atom-links';
import type {
  AtomicCodeMirrorEditorHandle,
  AtomicCodeMirrorEditorProps,
} from '@atomic-editor/editor';

// Lazy-load the editor module AND the curated code-languages
// registry together. Pinning both inside the same dynamic boundary
// keeps them in one lazy chunk, and wrapping the base component
// lets us pass the default `codeLanguages` without every call site
// having to know about the sub-path import.
const AtomicCodeMirrorEditor = lazy(async () => {
  const [mod, langs] = await Promise.all([
    import('@atomic-editor/editor'),
    import('@atomic-editor/editor/code-languages'),
  ]);
  const Base = mod.AtomicCodeMirrorEditor;
  const DEFAULT_LANGUAGES = langs.ATOMIC_CODE_LANGUAGES;
  const Wrapped = (props: AtomicCodeMirrorEditorProps) => (
    <Base
      {...props}
      codeLanguages={props.codeLanguages ?? DEFAULT_LANGUAGES}
    />
  );
  return { default: Wrapped };
});

function DossierMembershipControls({
  atom,
  onUpdated,
  onReload,
  onOpen,
}: {
  atom: AtomWithTags;
  onUpdated: (atom: AtomWithTags) => void;
  onReload: () => Promise<void>;
  onOpen: (id: string) => void;
}) {
  const [expanded, setExpanded] = useState(false);
  const [query, setQuery] = useState('');
  const [results, setResults] = useState<SemanticSearchResult[]>([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const members = atom.members ?? [];
  const readOnly = atom.anchor_role != null;

  const change = async (memoryId: string, attached: boolean) => {
    if (atom.summaries_revision == null) {
      setError('Dossier revision is unavailable. Reload and try again.');
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const updated = await getTransport().invoke<AtomWithTags>(
        attached ? 'attach_memu_category_memory' : 'detach_memu_category_memory',
        {
          categoryId: atom.id,
          memoryId,
          displayedSummary: atom.content,
          summariesRevision: atom.summaries_revision,
        },
      );
      useCanvasStore.getState().invalidateCanvasData();
      onUpdated(updated);
      setQuery('');
      setResults([]);
    } catch (reason) {
      setError(String(reason));
      await onReload().catch(() => undefined);
    } finally {
      setBusy(false);
    }
  };

  const search = async (event: FormEvent) => {
    event.preventDefault();
    if (!query.trim()) return;
    setError(null);
    try {
      setResults(await getTransport().invoke<SemanticSearchResult[]>('search_atoms_hybrid', {
        query,
        limit: 8,
        memoryOnly: true,
        excludeCategoryId: atom.id,
      }));
    } catch (reason) {
      setError(String(reason));
    }
  };

  return (
    <section className="mt-4 border-t border-[var(--color-border)] pt-4">
      <button type="button" onClick={() => setExpanded(value => !value)} className="flex w-full items-center justify-between text-left text-sm font-medium text-[var(--color-text-primary)]">
        <span>Memories in this dossier ({members.length})</span>
        <ChevronDown className={`h-4 w-4 transition-transform ${expanded ? 'rotate-180' : ''}`} />
      </button>
      {expanded && (
        <div className="mt-3 space-y-2">
          {readOnly && <p className="text-xs text-[var(--color-text-tertiary)]">Membership is managed by consolidation.</p>}
          {members.map(member => (
            <div key={member.memory_id} className="flex items-start gap-2 rounded border border-[var(--color-border)] p-2 text-xs">
              <button type="button" onClick={() => onOpen(member.id)} className="min-w-0 flex-1 text-left hover:text-[var(--color-accent)]">
                <span className="mr-2 text-[var(--color-text-tertiary)]">{member.memory_ref ?? 'Unnumbered'} · {member.status}</span>
                <span>{member.summary}</span>
              </button>
              {!readOnly && <button type="button" disabled={busy || member.cited} title={member.cited ? `Remove ${member.memory_ref} from dossier text first` : 'Detach memory'} onClick={() => void change(member.memory_id, false)} className="text-[var(--color-text-tertiary)] enabled:hover:text-red-400 disabled:cursor-not-allowed disabled:opacity-40">Detach</button>}
            </div>
          ))}
          {members.length === 0 && <p className="text-xs text-[var(--color-text-tertiary)]">No memories attached.</p>}
          {!readOnly && <form onSubmit={search} className="flex gap-2">
            <input value={query} onChange={event => setQuery(event.target.value)} placeholder="Find a memory or enter M#" className="min-w-0 flex-1 rounded border border-[var(--color-border)] bg-[var(--color-bg-primary)] px-2 py-1.5 text-xs outline-none focus:border-[var(--color-accent)]" />
            <button type="submit" disabled={busy} className="rounded border border-[var(--color-border)] px-2 py-1.5 text-xs disabled:opacity-50">Search</button>
          </form>}
          {!readOnly && results.map(memory => <button key={memory.id} type="button" disabled={busy} onClick={() => void change(memory.id, true)} className="block w-full rounded px-2 py-1.5 text-left text-xs hover:bg-[var(--color-bg-hover)] disabled:opacity-50">{memory.title || memory.snippet}</button>)}
          {error && <p className="text-xs text-red-400">{error}</p>}
        </div>
      )}
    </section>
  );
}

interface AtomReaderProps {
  atomId: string;
  highlightText?: string | null;
  initialEditing?: boolean;
}

export function AtomReader({ atomId, highlightText, initialEditing }: AtomReaderProps) {
  const deleteAtom = useAtomsStore(s => s.deleteAtom);
  const fetchTags = useTagsStore(s => s.fetchTags);
  const setSelectedTag = useUIStore(s => s.setSelectedTag);
  const overlayNavigate = useUIStore(s => s.overlayNavigate);
  const overlayDismiss = useUIStore(s => s.overlayDismiss);
  const removeAtomFromTabs = useUIStore(s => s.removeAtomFromTabs);
  const redirectAtomTabToFinding = useUIStore(s => s.redirectAtomTabToFinding);

  const [atom, setAtom] = useState<AtomWithTags | null>(null);
  const [isLoadingAtom, setIsLoadingAtom] = useState(true);
  const [showLoading, setShowLoading] = useState(false);
  const lastFetchedAt = useRef<string | null>(null);

  const refreshAtom = useCallback(async () => {
    const fetchedAtom = await getTransport().invoke<AtomWithTags | null>('get_atom_by_id', { id: atomId });
    setAtom(fetchedAtom);
    lastFetchedAt.current = fetchedAtom?.updated_at ?? null;
  }, [atomId]);


  // Watch the atoms store for updates to the currently viewed atom
  const storeAtom = useAtomsStore((s) =>
    s.atoms.find((a) => a.id === atomId)
  );

  // Fetch atom from database
  useEffect(() => {
    setIsLoadingAtom(true);
    setShowLoading(false);

    // Only show loading indicator if fetch takes longer than 200ms
    const loadingTimer = setTimeout(() => setShowLoading(true), 200);

    refreshAtom()
      .then(() => {
        clearTimeout(loadingTimer);
        setIsLoadingAtom(false);
      })
      .catch((error) => {
        clearTimeout(loadingTimer);
        console.error('Failed to fetch atom:', error);
        setAtom(null);
        setIsLoadingAtom(false);
        // atom loaded
      });

    return () => clearTimeout(loadingTimer);
  }, [atomId, refreshAtom]);

  // Re-fetch when store summary changes (e.g., after tag extraction)
  const storeAtomUpdatedAt = storeAtom?.updated_at;
  useEffect(() => {
    if (storeAtomUpdatedAt && !isLoadingAtom && storeAtomUpdatedAt !== lastFetchedAt.current) {
      lastFetchedAt.current = storeAtomUpdatedAt;
      refreshAtom().catch(console.error);
    }
  }, [storeAtomUpdatedAt, isLoadingAtom, refreshAtom]);

  // Refresh the open reader immediately when tagging completes for this atom.
  // The list store gets its status update from the global event hook, but the
  // reader owns full atom details and needs its own refresh to pick up new tags.
  useEffect(() => {
    const transport = getTransport();
    return transport.subscribe<{ atom_id: string }>('tagging-complete', (payload) => {
      if (payload.atom_id !== atomId) return;
      refreshAtom().catch(console.error);
    });
  }, [atomId, refreshAtom]);

  // If the fetched atom turns out to be a report finding (`kind = 'report'`),
  // redirect to the specialized FindingReader view. The generic atom reader
  // can't render `[N]` citation popovers, and findings are conceptually
  // read-only output not the user's own captures. This covers any path that
  // reached us via `/atoms/:id` for a finding atom — semantic search hits,
  // stale links from before the reports view existed, etc. The redirect
  // morphs the active tab in place + URL-replaces, so neither the tab
  // strip nor the browser back stack accumulates a dead /atoms/:id entry.
  useEffect(() => {
    if (atom?.kind === 'report') {
      redirectAtomTabToFinding(atomId);
    }
  }, [atom, atomId, redirectAtomTabToFinding]);

  return (
    <div className="h-full bg-[var(--color-bg-main)]">
      {isLoadingAtom ? (
        showLoading ? (
          <div className="flex items-center justify-center h-full text-[var(--color-text-secondary)]">
            Loading...
          </div>
        ) : null
      ) : !atom ? (
        <div className="flex items-center justify-center h-full text-[var(--color-text-secondary)]">
          Atom not found
        </div>
      ) : (
        <AtomReaderContent
          atom={atom}
          highlightText={highlightText}
          initialEditing={initialEditing}
          onDismiss={overlayDismiss}
          onDelete={async () => {
            if (atomId.startsWith('memory:')) {
              await getTransport().invoke('delete_memory', { id: atomId });
              useAtomsStore.setState((state) => {
                const atoms = state.atoms.filter((a) => a.id !== atomId);
                const semanticSearchResults = state.semanticSearchResults?.filter((a) => a.id !== atomId) ?? null;
                return {
                  atoms,
                  semanticSearchResults,
                  totalCount: atoms.length === state.atoms.length ? state.totalCount : Math.max(0, state.totalCount - 1),
                };
              });
            } else {
              await deleteAtom(atomId);
            }
            await fetchTags();
            removeAtomFromTabs(atomId);
          }}
          onTagClick={(tagId) => { setSelectedTag(tagId); overlayDismiss(); }}
          onRelatedAtomClick={(id, opts) => overlayNavigate({ type: 'reader', atomId: id }, opts)}
          onViewGraph={(opts) => overlayNavigate({ type: 'graph', atomId }, opts)}
          onAtomUpdated={(updated) => setAtom(updated)}
          onReload={refreshAtom}
        />
      )}
    </div>
  );
}

interface AtomReaderContentProps {
  atom: AtomWithTags;
  highlightText?: string | null;
  initialEditing?: boolean;
  onDismiss: () => void;
  onDelete: () => Promise<void>;
  onTagClick: (tagId: string) => void;
  onRelatedAtomClick: (atomId: string, opts?: { newTab?: boolean }) => void;
  onViewGraph: (opts?: { newTab?: boolean }) => void;
  onAtomUpdated?: (atom: AtomWithTags) => void;
  onReload: () => Promise<void>;
}

function AtomReaderContent({
  atom, highlightText, initialEditing,
  onDismiss, onDelete, onTagClick, onRelatedAtomClick, onViewGraph, onAtomUpdated, onReload,
}: AtomReaderContentProps) {
  const readerTheme = useUIStore(s => s.readerTheme);
  const setReaderEditState = useUIStore(s => s.setReaderEditState);
  const retryTagging = useAtomsStore(s => s.retryTagging);
  const containerRef = useRef<HTMLDivElement | null>(null);
  const editorHandleRef = useRef<AtomicCodeMirrorEditorHandle | null>(null);
  const [showDeleteModal, setShowDeleteModal] = useState(false);
  const [isDeleting, setIsDeleting] = useState(false);
  const [showTagSelector, setShowTagSelector] = useState(false);
  const isMemuMemory = atom.id.startsWith('memory:');
  const isMemuCategory = atom.id.startsWith('category:');
  const isMemuEntity = atom.id.startsWith('entity:');
  const isMemuAtom = isMemuMemory || isMemuCategory || isMemuEntity;
  const [memuTitle, setMemuTitle] = useState(atom.title);
  const [memuDescription, setMemuDescription] = useState(atom.description ?? '');
  const [memuSummary, setMemuSummary] = useState(atom.content);
  const [memuEditing, setMemuEditing] = useState(Boolean(initialEditing));
  const [memuStatus, setMemuStatus] = useState<'idle' | 'saving'>('idle');
  const [memuError, setMemuError] = useState<string | null>(null);
  const memuSummaryEdited = memuSummary !== atom.content;
  const memuCategoryEdited = isMemuCategory && (
    memuTitle !== atom.title || memuDescription !== (atom.description ?? '')
  );
  const memuEdited = memuSummaryEdited || memuCategoryEdited;
  const memuSummaryApproved = isMemuMemory
    ? Boolean(atom.approved_at)
    : isMemuCategory
      && atom.approved_summary === atom.content
      && atom.approved_description === (atom.description ?? '');
  const memuPrimaryDisabled = memuStatus !== 'idle' || (memuSummaryApproved && !memuEdited);
  const memuPrimaryLabel = memuStatus === 'saving'
    ? 'Saving...'
    : (memuSummaryApproved ? 'Save' : (memuEdited ? 'Save + approve' : 'Approve'));

  const {
    editContent, editSourceUrl, editTags, saveStatus,
    editorRevision,
    startEditing, setEditContent, setEditSourceUrl, setEditTags, saveNow, flushDraft,
  } = useInlineEditor({ atom, onAtomUpdated, readOnly: isMemuAtom });
  const isTaggingInFlight = atom.tagging_status === 'pending' || atom.tagging_status === 'processing';

  useEffect(() => {
    setMemuTitle(atom.title);
    setMemuDescription(atom.description ?? '');
    setMemuSummary(atom.content);
    setMemuEditing(Boolean(initialEditing));
    setMemuError(null);
  }, [atom.id, atom.title, atom.description, atom.content, initialEditing]);

  const handleAutoTag = useCallback(async () => {
    await retryTagging(atom.id);
    onAtomUpdated?.({ ...atom, tagging_status: 'pending' });
  }, [retryTagging, atom, onAtomUpdated]);

  const saveMemuSummary = useCallback(async () => {
    if (!isMemuMemory && !isMemuCategory) return;
    const changes = {
      ...(memuSummaryEdited ? { summary: memuSummary } : {}),
      ...(memuCategoryEdited && memuTitle !== atom.title ? { title: memuTitle } : {}),
      ...(memuCategoryEdited && memuDescription !== (atom.description ?? '') ? { description: memuDescription } : {}),
    };
    if (Object.keys(changes).length === 0) return;
    if (isMemuCategory && atom.summaries_revision == null) {
      setMemuError('Category changed or its revision is unavailable. Reload and try again.');
      return;
    }
    setMemuStatus('saving');
    setMemuError(null);
    try {
      const updated = await getTransport().invoke<AtomWithTags>(
        isMemuMemory ? 'update_memory_summary' : 'update_category_summary',
        {
          id: atom.id,
          ...changes,
          ...(isMemuCategory ? {
            displayed_summary: atom.content,
            summaries_revision: atom.summaries_revision,
          } : {}),
        },
      );
      useCanvasStore.getState().invalidateCanvasData();
      onAtomUpdated?.(updated);
      if (isMemuCategory) setMemuEditing(false);
    } catch (error) {
      setMemuError(String(error));
    } finally {
      setMemuStatus('idle');
    }
  }, [atom, isMemuCategory, isMemuMemory, memuCategoryEdited, memuDescription, memuSummary, memuSummaryEdited, memuTitle, onAtomUpdated]);

  const approveMemuSummary = useCallback(async () => {
    if (!isMemuMemory && !isMemuCategory) return;
    if (isMemuCategory && atom.summaries_revision == null) {
      setMemuError('Category changed or its revision is unavailable. Reload and try again.');
      return;
    }
    setMemuStatus('saving');
    setMemuError(null);
    try {
      const updated = await getTransport().invoke<AtomWithTags>(
        isMemuMemory ? 'approve_memory' : 'approve_category',
        {
          id: atom.id,
          ...(isMemuCategory ? {
            displayed_summary: atom.content,
            summaries_revision: atom.summaries_revision,
          } : {}),
        },
      );
      useCanvasStore.getState().invalidateCanvasData();
      onAtomUpdated?.(updated);
    } catch (error) {
      setMemuError(String(error));
    } finally {
      setMemuStatus('idle');
    }
  }, [atom, isMemuCategory, isMemuMemory, onAtomUpdated]);

  useEffect(() => {
    setReaderEditState(Boolean(initialEditing), saveStatus);
    return () => {
      setReaderEditState(false, 'idle');
    };
  }, [initialEditing, saveStatus, setReaderEditState]);

  useEffect(() => {
    if (!isMemuAtom) startEditing();
  }, [isMemuAtom, startEditing]);

  useEffect(() => {
    if (!initialEditing) return;
    const id = requestAnimationFrame(() => {
      editorHandleRef.current?.focus();
    });
    return () => cancelAnimationFrame(id);
  }, [initialEditing]);

  useEffect(() => {
    if (initialEditing) return;
    containerRef.current?.focus({ preventScroll: true });
  }, [initialEditing, atom.id]);

  useEffect(() => {
    readerEditorActions.current = {
      startEditing: () => {
        editorHandleRef.current?.focus();
      },
      stopEditing: async () => {
        await flushDraft();
      },
      undo: () => editorHandleRef.current?.undo(),
      redo: () => editorHandleRef.current?.redo(),
      openSearch: (query?: string) => editorHandleRef.current?.openSearch(query),
      closeSearch: () => editorHandleRef.current?.closeSearch(),
    };
    return () => {
      readerEditorActions.current = null;
    };
  }, [flushDraft]);

  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      if (e.key === 'Escape' && editorHandleRef.current?.isSearchOpen()) {
        e.preventDefault();
        readerEditorActions.current?.closeSearch();
        return;
      }

      if ((e.ctrlKey || e.metaKey) && e.key === 's') {
        e.preventDefault();
        void (isMemuAtom ? (memuEdited ? saveMemuSummary() : (!memuSummaryApproved && approveMemuSummary())) : saveNow());
        return;
      }
      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'f') {
        e.preventDefault();
        editorHandleRef.current?.openSearch();
        return;
      }
      if (e.key === 'Escape' && !showDeleteModal) {
        e.preventDefault();
        void (async () => {
          await flushDraft();
          onDismiss();
        })();
      }
    };
    document.addEventListener('keydown', handleKeyDown);
    return () => document.removeEventListener('keydown', handleKeyDown);
  }, [approveMemuSummary, flushDraft, isMemuAtom, memuEdited, memuSummaryApproved, onDismiss, saveMemuSummary, saveNow, showDeleteModal]);

  const [revealed, setRevealed] = useState(false);
  useEffect(() => {
    const frame = requestAnimationFrame(() => setRevealed(true));
    return () => cancelAnimationFrame(frame);
  }, []);

  const handleDelete = async () => {
    setIsDeleting(true);
    setMemuError(null);
    try {
      await onDelete();
      setShowDeleteModal(false);
    } catch (error) {
      setMemuError(String(error));
    } finally {
      setIsDeleting(false);
    }
  };

  const suggestAtomLinks = useCallback(async (query: string): Promise<AtomLinkSuggestion[]> => {
    const trimmed = query.trim();
    const limit = 12;
    const transport = getTransport();

    const titleMatches = await transport.invoke<AtomLinkSuggestion[]>('get_atom_link_suggestions', {
      q: trimmed,
      limit,
    });
    const titleSuggestions = titleMatches
      .filter((suggestion) => suggestion.id !== atom.id)
      .map((suggestion) => ({
        ...suggestion,
        source: (suggestion.source ?? (trimmed ? 'title' : 'recent')) as AtomLinkSuggestionSource,
      }));

    if (!trimmed || titleSuggestions.length > 0) return titleSuggestions;

    const keywordMatches = await transport.invoke<SemanticSearchResult[]>('search_atoms_keyword', {
      query: trimmed,
      limit,
    });
    const keywordSuggestions = searchResultsToAtomLinkSuggestions(keywordMatches, atom.id, 'content');
    if (keywordSuggestions.length > 0) return keywordSuggestions;

    try {
      const hybridMatches = await transport.invoke<SemanticSearchResult[]>('search_atoms_hybrid', {
        query: trimmed,
        limit,
        threshold: 0.3,
      });
      return searchResultsToAtomLinkSuggestions(hybridMatches, atom.id, 'hybrid');
    } catch (error) {
      console.warn('Atom link hybrid fallback failed:', error);
      return [];
    }
  }, [atom.id]);

  const resolveAtomLink = useCallback(async (id: string) => {
    const linkedAtom = await getTransport().invoke<AtomWithTags | null>('get_atom_by_id', { id });
    if (!linkedAtom) return null;
    return {
      id: linkedAtom.id,
      title: linkedAtom.title,
      snippet: linkedAtom.snippet,
    };
  }, []);

  const atomLinkExtensions = useMemo(
    () => atomLinkExtension({
      currentAtomId: atom.id,
      suggestAtoms: suggestAtomLinks,
      resolveAtom: resolveAtomLink,
      openAtom: (id, opts) => onRelatedAtomClick(id, opts),
    }),
    [atom.id, onRelatedAtomClick, resolveAtomLink, suggestAtomLinks],
  );

  return (
    <div
      ref={containerRef}
      tabIndex={-1}
      data-reader-theme={readerTheme}
      className={`h-full flex flex-col bg-[var(--color-bg-main)] transition-opacity duration-300 ease-out focus:outline-none ${
        revealed ? 'opacity-100' : 'opacity-0'
      }`}
    >
      {/* @container makes the two-column layout react to the actual reader
          pane width rather than the viewport. With the chat sidebar open,
          the viewport may be wide while the reader is narrow — without
          container queries the desktop two-column would render at ~600px
          and squeeze the editor. */}
      <div className="@container flex-1 overflow-y-auto scrollbar-auto-hide">
        <div className="max-w-6xl mx-auto px-3 py-5 sm:px-4 sm:py-6 @4xl:px-6 @4xl:flex @4xl:gap-10">
          <div className="flex-1 min-w-0">
            {isMemuAtom ? (
              <div className="space-y-4">
                {isMemuMemory || isMemuCategory ? (
                  <>
                    {isMemuCategory && (
                      <div className="flex flex-wrap items-center gap-2 text-xs text-[var(--color-text-tertiary)]">
                        <span className="rounded-full border border-[var(--color-border)] px-2 py-0.5 capitalize">{atom.category_kind ?? 'topic'}</span>
                        <span>{atom.active === false ? 'Inactive' : 'Active'}</span>
                        {atom.last_evidence_at && <span title={formatDate(atom.last_evidence_at)}>Evidence: {formatDate(atom.last_evidence_at)}</span>}
                        {atom.last_revised_at && <span title={formatDate(atom.last_revised_at)}>Revised: {formatDate(atom.last_revised_at)}</span>}
                      </div>
                    )}
                    {isMemuMemory || memuEditing ? (
                      <div className="space-y-3">
                        {isMemuCategory && (
                          <>
                            <Input value={memuTitle} onChange={(e) => setMemuTitle(e.target.value)} placeholder="Category title" />
                            <textarea
                              value={memuDescription}
                              onChange={(e) => setMemuDescription(e.target.value)}
                              className="min-h-24 w-full resize-y rounded-lg border border-[var(--color-border)] bg-[var(--color-bg-card)] p-3 text-sm leading-6 text-[var(--color-text-primary)] focus:border-[var(--color-accent)] focus:outline-none"
                              placeholder="Description"
                            />
                          </>
                        )}
                        <textarea
                          value={memuSummary}
                          onChange={(e) => setMemuSummary(e.target.value)}
                          className="min-h-[24rem] w-full resize-y rounded-lg border border-[var(--color-border)] bg-[var(--color-bg-card)] p-4 text-sm leading-6 text-[var(--color-text-primary)] focus:border-[var(--color-accent)] focus:outline-none"
                        />
                      </div>
                    ) : (
                      <>
                        <h1 className="text-2xl font-semibold text-[var(--color-text-primary)]">{atom.title}</h1>
                        {atom.description && <p className="text-sm leading-6 text-[var(--color-text-secondary)]">{atom.description}</p>}
                        <div className="prose prose-sm prose-invert max-w-none leading-6 prose-headings:text-[var(--color-text-primary)] prose-p:text-[var(--color-text-primary)] prose-strong:text-[var(--color-text-primary)] prose-li:text-[var(--color-text-primary)]">
                          <DossierMarkdown citations={atom.citations}>{atom.content}</DossierMarkdown>
                        </div>
                      </>
                    )}
                    {memuError && (
                      <p className="rounded border border-red-500/40 bg-red-500/10 p-2 text-sm text-red-500">{memuError}</p>
                    )}
                    {isMemuCategory && memuEditing && <MemoryCitationLinks citations={atom.citations} />}
                    <div className="flex flex-wrap gap-2">
                      <button
                        onClick={() => void (memuEdited ? saveMemuSummary() : approveMemuSummary())}
                        disabled={memuPrimaryDisabled}
                        className="rounded bg-[var(--color-accent)] px-3 py-1.5 text-sm text-white transition enabled:hover:brightness-110 enabled:focus-visible:outline enabled:focus-visible:outline-2 enabled:focus-visible:outline-offset-2 enabled:focus-visible:outline-[var(--color-accent)] disabled:cursor-not-allowed disabled:opacity-[0.45]"
                      >
                        {memuPrimaryLabel}
                      </button>
                      {isMemuCategory && (memuEditing ? (
                        <button
                          onClick={() => {
                            setMemuTitle(atom.title);
                            setMemuDescription(atom.description ?? '');
                            setMemuSummary(atom.content);
                            setMemuEditing(false);
                          }}
                          disabled={memuStatus !== 'idle'}
                          className="rounded border border-[var(--color-border)] px-3 py-1.5 text-sm"
                        >
                          Cancel
                        </button>
                      ) : (
                        <button
                          onClick={() => setMemuEditing(true)}
                          className="rounded border border-[var(--color-border)] px-3 py-1.5 text-sm"
                        >
                          Edit
                        </button>
                      ))}
                      {isMemuMemory ? (
                        <button
                          onClick={() => setShowDeleteModal(true)}
                          disabled={memuStatus !== 'idle'}
                          className="rounded border border-red-500/50 px-3 py-1.5 text-sm text-red-500 transition-colors enabled:hover:border-red-500 enabled:hover:bg-red-500/10 enabled:focus-visible:outline enabled:focus-visible:outline-2 enabled:focus-visible:outline-offset-2 enabled:focus-visible:outline-red-500 disabled:cursor-not-allowed disabled:opacity-[0.45]"
                        >
                          Delete
                        </button>
                      ) : (
                        <button disabled title="Delete not implemented" className="rounded border border-[var(--color-border)] px-3 py-1.5 text-sm opacity-50">
                          Delete
                        </button>
                      )}
                    </div>
                  </>
                ) : (
                  <div className="prose prose-invert max-w-none prose-headings:text-[var(--color-text-primary)] prose-p:text-[var(--color-text-primary)] prose-a:text-[var(--color-text-primary)] prose-a:underline prose-a:decoration-[var(--color-border-hover)] prose-strong:text-[var(--color-text-primary)] prose-code:text-[var(--color-accent-light)] prose-code:bg-[var(--color-bg-card)] prose-code:px-1 prose-code:py-0.5 prose-code:rounded prose-pre:bg-[var(--color-bg-card)] prose-pre:border prose-pre:border-[var(--color-border)] prose-blockquote:border-l-[var(--color-accent)] prose-blockquote:text-[var(--color-text-secondary)] prose-li:text-[var(--color-text-primary)]">
                    <DossierMarkdown>{atom.content}</DossierMarkdown>
                  </div>
                )}
              </div>
            ) : (
              <Suspense fallback={null}>
                <AtomicCodeMirrorEditor
                  key={`${atom.id}:${editorRevision}`}
                  documentId={atom.id}
                  markdownSource={editContent}
                  initialRevealText={highlightText}
                  blurEditorOnMount={!initialEditing}
                  onMarkdownChange={setEditContent}
                  onLinkClick={(url) => {
                    void openExternalUrl(url);
                  }}
                  editorHandleRef={editorHandleRef}
                  extensions={atomLinkExtensions}
                />
              </Suspense>
            )}
          </div>

          <div className="w-full @4xl:w-80 @4xl:shrink-0 mt-6 @4xl:mt-0 border border-[var(--color-border)] rounded-lg p-4 self-start">
            <div className="mb-4">
              {/* Source URL + delete share a row — delete sits to the right
                  of the input. */}
              <div className="flex items-center gap-1.5">
                <div className="flex-1 min-w-0">
                  <Input
                    value={editSourceUrl}
                    onChange={(e) => setEditSourceUrl(e.target.value)}
                    placeholder="Source URL (optional)"
                    className="text-xs"
                    disabled={isMemuAtom}
                  />
                </div>
                <button
                  onClick={() => setShowDeleteModal(true)}
                  disabled={isMemuAtom}
                  className="shrink-0 p-1.5 rounded text-[var(--color-text-secondary)] hover:text-red-400 hover:bg-[var(--color-bg-hover)] transition-colors"
                  title="Delete atom"
                  aria-label="Delete atom"
                >
                  <Trash2 className="w-3.5 h-3.5" strokeWidth={2} />
                </button>
              </div>
              {atom.source_url && (
                <button
                  type="button"
                  onClick={() => {
                    void openExternalUrl(atom.source_url!);
                  }}
                  className="mt-2 inline-block text-xs text-[var(--color-text-tertiary)] hover:text-[var(--color-accent)]"
                >
                  Open source
                </button>
              )}
            </div>

            <div className="mb-4">
              <div className="flex flex-wrap gap-1.5 mb-2">
                {editTags.map((tag) => (
                  <TagChip
                    key={tag.id}
                    name={tag.name}
                    size="sm"
                    onRemove={isMemuAtom ? undefined : () => setEditTags(editTags.filter((t) => t.id !== tag.id))}
                    onClick={() => onTagClick(tag.id)}
                  />
                ))}
                {!isMemuAtom && <button
                  onClick={() => setShowTagSelector(!showTagSelector)}
                  className="text-xs text-[var(--color-accent)] hover:text-[var(--color-accent-light)] transition-colors px-1.5 py-0.5 rounded border border-dashed border-[var(--color-border)]"
                >
                  +
                </button>}
              </div>
              {!isMemuAtom && showTagSelector && (
                <TagSelector selectedTags={editTags} onTagsChange={setEditTags} />
              )}
            </div>

            {!isMemuAtom && editTags.length === 0 && (
              <div className="mb-4 rounded-xl border border-dashed border-[var(--color-border)] bg-[var(--color-bg-card)]/60 p-3">
                <div className="flex items-center justify-between gap-3">
                  <div className="min-w-0">
                    <p className="text-sm text-[var(--color-text-primary)]">No tags yet</p>
                    <p className="mt-1 text-xs text-[var(--color-text-tertiary)]">
                      Run tagging for this atom manually.
                    </p>
                  </div>
                  <button
                    onClick={() => {
                      void handleAutoTag();
                    }}
                    disabled={isTaggingInFlight}
                    className="shrink-0 rounded-lg border border-[var(--color-border)] bg-[var(--color-bg-elevated)] px-3 py-1.5 text-xs font-medium text-[var(--color-text-primary)] transition-colors hover:border-[var(--color-accent)] hover:text-[var(--color-accent)] disabled:cursor-not-allowed disabled:opacity-60"
                  >
                    {isTaggingInFlight ? 'Tagging...' : 'Auto-tag'}
                  </button>
                </div>
              </div>
            )}

            {isMemuMemory && (
              <MemoryEntityControls
                memoryId={atom.id}
                entityIds={atom.entity_ids}
                entityNames={atom.entity_names}
                onUpdated={updated => {
                  useCanvasStore.getState().invalidateCanvasData();
                  onAtomUpdated?.(updated);
                }}
              />
            )}

            {isMemuCategory && (
              <DossierMembershipControls
                atom={atom}
                onUpdated={(updated) => onAtomUpdated?.(updated)}
                onReload={onReload}
                onOpen={(id) => onRelatedAtomClick(id)}
              />
            )}

            {/* Dates */}
            <div className="text-xs text-[var(--color-text-tertiary)] space-y-0.5">
              {atom.published_at && <p>{formatDate(atom.published_at)}</p>}
              <p>{formatDate(atom.updated_at)}</p>
            </div>

            {/* Neighborhood graph — always visible */}
            {atom.embedding_status !== 'failed' && (
              <div className="mt-4">
                <MiniGraphPreview atomId={atom.id} onExpand={onViewGraph} />
              </div>
            )}

            {/* Related atoms — collapsible */}
            {atom.embedding_status !== 'failed' && (
              <SidebarRelatedAtoms atomId={atom.id} onAtomClick={onRelatedAtomClick} />
            )}
          </div>
        </div>
      </div>

      {/* Delete Confirmation Modal */}
      <Modal
        isOpen={showDeleteModal}
        onClose={() => setShowDeleteModal(false)}
        title={isMemuMemory ? 'Delete Memory' : 'Delete Atom'}
        confirmLabel={isDeleting ? 'Deleting...' : 'Delete'}
        confirmVariant="danger"
        onConfirm={handleDelete}
      >
        <p>Are you sure you want to delete this {isMemuMemory ? 'memory' : 'atom'}? This action cannot be undone.</p>
        {memuError && (
          <p className="mt-3 rounded border border-red-500/40 bg-red-500/10 p-2 text-sm text-red-500">{memuError}</p>
        )}
      </Modal>
    </div>
  );
}

function searchResultsToAtomLinkSuggestions(
  results: SemanticSearchResult[],
  currentAtomId: string,
  source: AtomLinkSuggestionSource,
): AtomLinkSuggestion[] {
  return results
    .filter((result) => result.id !== currentAtomId)
    .map((result) => ({
      id: result.id,
      title: result.title,
      snippet: result.matching_chunk_content || result.snippet,
      source,
    }));
}

function SidebarRelatedAtoms({ atomId, onAtomClick }: { atomId: string; onAtomClick: (id: string, opts?: { newTab?: boolean }) => void }) {
  const [relatedAtoms, setRelatedAtoms] = useState<SimilarAtomResult[]>([]);
  const [isCollapsed, setIsCollapsed] = useState(true);
  const [hasLoaded, setHasLoaded] = useState(false);
  const [isLoading, setIsLoading] = useState(false);

  useEffect(() => {
    let cancelled = false;
    setRelatedAtoms([]);
    setHasLoaded(false);
    setIsLoading(true);
    findSimilarAtoms(atomId, 5, 0.6)
      .then((results) => {
        if (cancelled) return;
        setRelatedAtoms(results);
        setHasLoaded(true);
      })
      .catch((error) => {
        if (cancelled) return;
        console.error(error);
        setRelatedAtoms([]);
        setHasLoaded(true);
      })
      .finally(() => {
        if (!cancelled) setIsLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [atomId]);

  return (
    <div className="mt-4">
      <button
        onClick={() => setIsCollapsed(!isCollapsed)}
        className="flex items-center justify-between w-full text-xs font-medium text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] transition-colors"
      >
        <span>Related atoms</span>
        <ChevronDown className={`w-3 h-3 transition-transform ${isCollapsed ? '' : 'rotate-180'}`} strokeWidth={2} />
      </button>
      {!isCollapsed && (
        <div className="mt-2 space-y-1.5">
          {isLoading ? (
            <div className="text-xs text-[var(--color-text-tertiary)]">Loading...</div>
          ) : relatedAtoms.length > 0 ? (
            relatedAtoms.map((result) => (
              <button
                key={result.id}
                onClick={(e) => onAtomClick(result.id, { newTab: e.metaKey || e.ctrlKey })}
                onAuxClick={(e) => {
                  if (e.button === 1) {
                    e.preventDefault();
                    onAtomClick(result.id, { newTab: true });
                  }
                }}
                className="w-full text-left p-2 rounded-md hover:bg-[var(--color-bg-hover)] transition-colors"
              >
                <p className="text-xs text-[var(--color-text-primary)] line-clamp-2">
                  {result.title || 'Untitled'}
                </p>
                <span className="text-[10px] text-[var(--color-accent)]">
                  {Math.round(result.similarity_score * 100)}% similar
                </span>
              </button>
            ))
          ) : hasLoaded ? (
            <div className="text-xs text-[var(--color-text-tertiary)]">No similar atoms found</div>
          ) : null}
        </div>
      )}
    </div>
  );
}
