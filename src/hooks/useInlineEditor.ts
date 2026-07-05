import { useState, useRef, useCallback, useEffect } from 'react';
import { useAtomsStore, type AtomWithTags, type Tag } from '../stores/atoms';
import { useTagsStore } from '../stores/tags';
import { useUIStore } from '../stores/ui';

const AUTO_SAVE_DELAY = 1500; // ms

export type SaveStatus = 'idle' | 'saving' | 'saved' | 'error';

interface UseInlineEditorOptions {
  atom: AtomWithTags;
  onAtomUpdated?: (atom: AtomWithTags) => void;
  readOnly?: boolean;
}

interface UseInlineEditorReturn {
  isEditing: boolean;
  isTransitioning: boolean;
  editContent: string;
  editSourceUrl: string;
  editTags: Tag[];
  saveStatus: SaveStatus;
  cursorOffset: number | null;
  editorRevision: number;

  startEditing: (cursorOffset?: number) => void;
  stopEditing: () => Promise<void>;
  setEditContent: (content: string) => void;
  setEditSourceUrl: (url: string) => void;
  setEditTags: (tags: Tag[]) => void;
  saveNow: () => Promise<void>;
  flushDraft: () => Promise<void>;
}

export function useInlineEditor({
  atom,
  onAtomUpdated,
  readOnly = false,
}: UseInlineEditorOptions): UseInlineEditorReturn {
  const updateAtomContentOnly = useAtomsStore(s => s.updateAtomContentOnly);
  const processAtomPipeline = useAtomsStore(s => s.processAtomPipeline);
  const deleteAtom = useAtomsStore(s => s.deleteAtom);
  const fetchTags = useTagsStore(s => s.fetchTags);

  // Track if the atom was created empty (new atom flow)
  const wasCreatedEmpty = useRef(!atom.content.trim());

  const [isEditing, setIsEditing] = useState(false);
  const [isTransitioning, setIsTransitioning] = useState(false);
  const [editContent, setEditContent] = useState(atom.content);
  const [editSourceUrl, setEditSourceUrl] = useState(atom.source_url || '');
  const [editTags, setEditTags] = useState<Tag[]>(atom.tags);
  const [saveStatus, setSaveStatus] = useState<SaveStatus>('idle');
  const [cursorOffset, setCursorOffset] = useState<number | null>(null);
  const [editorRevision, setEditorRevision] = useState(0);

  const debounceRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const isSavingRef = useRef(false);
  const isEditingRef = useRef(false);
  const atomIdRef = useRef(atom.id);
  const savingPromiseRef = useRef<Promise<void>>(Promise.resolve());
  const needsPipelineRef = useRef(false);
  const editContentRef = useRef(editContent);
  const editSourceUrlRef = useRef(editSourceUrl);
  const editTagsRef = useRef(editTags);
  // Track what was last saved to detect dirty state
  const lastSavedRef = useRef({
    content: atom.content,
    sourceUrl: atom.source_url || '',
    tagIds: atom.tags.map(t => t.id).sort().join(','),
  });

  // Keep refs in sync
  useEffect(() => {
    isEditingRef.current = isEditing;
  }, [isEditing]);
  useEffect(() => { editContentRef.current = editContent; }, [editContent]);
  useEffect(() => { editSourceUrlRef.current = editSourceUrl; }, [editSourceUrl]);
  useEffect(() => { editTagsRef.current = editTags; }, [editTags]);

  // Sync from atom prop for external updates. The CodeMirror editor consumes
  // markdownSource only on mount, so a real incoming content/source/tag change
  // bumps editorRevision and remounts the editor. Own autosaves update atom
  // metadata but match lastSavedRef, so they do not remount and disrupt typing.
  useEffect(() => {
    const incoming = {
      content: atom.content,
      sourceUrl: atom.source_url || '',
      tagIds: atom.tags.map(t => t.id).sort().join(','),
    };
    const atomChanged = atom.id !== atomIdRef.current;
    const matchesLastSaved =
      incoming.content === lastSavedRef.current.content &&
      incoming.sourceUrl === lastSavedRef.current.sourceUrl &&
      incoming.tagIds === lastSavedRef.current.tagIds;

    if (!atomChanged && matchesLastSaved) return;

    const currentTagIds = editTagsRef.current.map(t => t.id).sort().join(',');
    const hasLocalDraftChanges =
      editContentRef.current !== lastSavedRef.current.content ||
      editSourceUrlRef.current !== lastSavedRef.current.sourceUrl ||
      currentTagIds !== lastSavedRef.current.tagIds;

    if (!atomChanged && hasLocalDraftChanges) {
      return;
    }

    const shouldRemountEditor = atomChanged || incoming.content !== editContentRef.current;
    atomIdRef.current = atom.id;
    setEditContent(atom.content);
    setEditSourceUrl(atom.source_url || '');
    setEditTags(atom.tags);
    editContentRef.current = atom.content;
    editSourceUrlRef.current = atom.source_url || '';
    editTagsRef.current = atom.tags;
    lastSavedRef.current = incoming;
    if (shouldRemountEditor) {
      setEditorRevision((revision) => revision + 1);
    }
  }, [atom.id, atom.content, atom.source_url, atom.tags]);

  const isDirty = useCallback(() => {
    const currentTagIds = editTags.map(t => t.id).sort().join(',');
    return (
      editContent !== lastSavedRef.current.content ||
      editSourceUrl !== lastSavedRef.current.sourceUrl ||
      currentTagIds !== lastSavedRef.current.tagIds
    );
  }, [editContent, editSourceUrl, editTags]);

  const hasPipelineRelevantChanges = useCallback(() => {
    return (
      editContentRef.current !== lastSavedRef.current.content ||
      editSourceUrlRef.current !== lastSavedRef.current.sourceUrl
    );
  }, []);

  /** Content-only save (no pipeline). */
  const doContentSave = useCallback(async () => {
    if (readOnly) return;
    if (isSavingRef.current) return;
    isSavingRef.current = true;
    setSaveStatus('saving');
    const promise = (async () => {
      try {
        const content = editContentRef.current;
        const sourceUrl = editSourceUrlRef.current;
        const tags = editTagsRef.current;
        const tagIds = tags.map(t => t.id);
        const needsPipelineForThisSave = hasPipelineRelevantChanges();
        const saved = await updateAtomContentOnly(
          atom.id,
          content,
          sourceUrl || undefined,
          tagIds,
        );
        lastSavedRef.current = {
          content,
          sourceUrl,
          tagIds: tagIds.sort().join(','),
        };
        if (needsPipelineForThisSave) {
          needsPipelineRef.current = true;
        }
        setSaveStatus('saved');
        onAtomUpdated?.(saved);
      } catch {
        setSaveStatus('error');
      } finally {
        isSavingRef.current = false;
      }
    })();
    savingPromiseRef.current = promise;
    await promise;
  }, [atom.id, hasPipelineRelevantChanges, readOnly, updateAtomContentOnly, onAtomUpdated]);

  /** Flush latest draft and only kick the pipeline when content/source changed. */
  const finalizeDraft = useCallback(async () => {
    if (readOnly) return;
    // Wait for any in-flight content-only save to complete first
    await savingPromiseRef.current;
    setSaveStatus('saving');
    try {
      if (isDirty()) {
        await doContentSave();
      }
      if (needsPipelineRef.current) {
        await processAtomPipeline(atom.id);
        needsPipelineRef.current = false;
      }
      setSaveStatus('saved');
      await fetchTags();
    } catch {
      setSaveStatus('error');
    }
  }, [atom.id, isDirty, readOnly, doContentSave, processAtomPipeline, fetchTags]);

  /** Schedule a debounced content-only save. */
  const scheduleSave = useCallback(() => {
    if (readOnly) return;
    if (debounceRef.current) clearTimeout(debounceRef.current);
    debounceRef.current = setTimeout(() => {
      debounceRef.current = null;
      doContentSave();
    }, AUTO_SAVE_DELAY);
  }, [doContentSave, readOnly]);

  /** Wrappers that schedule auto-save on change. */
  const handleSetContent = useCallback((content: string) => {
    setEditContent(content);
    editContentRef.current = content;
    scheduleSave();
  }, [scheduleSave]);

  const handleSetSourceUrl = useCallback((url: string) => {
    setEditSourceUrl(url);
    editSourceUrlRef.current = url;
    scheduleSave();
  }, [scheduleSave]);

  const handleSetTags = useCallback((tags: Tag[]) => {
    setEditTags(tags);
    editTagsRef.current = tags;
    scheduleSave();
  }, [scheduleSave]);

  const startEditing = useCallback((offset?: number) => {
    if (readOnly) return;
    needsPipelineRef.current = false;
    setIsEditing(true);
    setCursorOffset(offset ?? null);
    setSaveStatus('idle');
  }, [readOnly]);

  const stopEditing = useCallback(async () => {
    if (readOnly) return;
    // Cancel pending debounced save
    if (debounceRef.current) {
      clearTimeout(debounceRef.current);
      debounceRef.current = null;
    }

    // If atom was created empty and still has no content, delete it.
    // removeAtomFromTabs both closes the now-orphaned tab AND navigates to
    // the atoms list — so we don't need a separate dismiss call.
    if (wasCreatedEmpty.current && !editContent.trim()) {
      deleteAtom(atom.id).catch(console.error);
      fetchTags().catch(console.error);
      useUIStore.getState().removeAtomFromTabs(atom.id);
      return;
    }

    await new Promise<void>((resolve) => {
      // Phase 1: blur the editor content (rendered this frame)
      setIsTransitioning(true);

      // Phase 2: after the blur paints, save + swap to view mode
      requestAnimationFrame(() => {
        const finish = () => {
          setIsEditing(false);
          setCursorOffset(null);
          // Phase 3: after the rendered markdown mounts, clear blur
          requestAnimationFrame(() => {
            setIsTransitioning(false);
            resolve();
          });
        };
        if (isDirty() || needsPipelineRef.current) {
          finalizeDraft().then(finish, finish);
        } else {
          finish();
        }
      });
    });
  }, [isDirty, finalizeDraft, editContent, atom.id, readOnly, deleteAtom, fetchTags]);

  /** Immediate content-only save (for Cmd+S). */
  const saveNow = useCallback(async () => {
    if (readOnly) return;
    if (debounceRef.current) {
      clearTimeout(debounceRef.current);
      debounceRef.current = null;
    }
    if (isDirty()) {
      await doContentSave();
    }
  }, [isDirty, doContentSave, readOnly]);

  const flushDraft = useCallback(async () => {
    if (readOnly) return;
    if (debounceRef.current) {
      clearTimeout(debounceRef.current);
      debounceRef.current = null;
    }
    if (isDirty() || needsPipelineRef.current) {
      await finalizeDraft();
    }
  }, [isDirty, finalizeDraft, readOnly]);

  // Cleanup on unmount: delete if empty, otherwise save
  useEffect(() => {
    return () => {
      if (readOnly) return;
      if (debounceRef.current) {
        clearTimeout(debounceRef.current);
      }
      if (isEditingRef.current) {
        const content = editContentRef.current;
        const sourceUrl = editSourceUrlRef.current;
        const latestTags = editTagsRef.current;
        const tagIds = latestTags.map(t => t.id).sort().join(',');
        const hasDraftChanges =
          content !== lastSavedRef.current.content ||
          sourceUrl !== lastSavedRef.current.sourceUrl ||
          tagIds !== lastSavedRef.current.tagIds;
        if (wasCreatedEmpty.current && !content.trim()) {
          // Never had content — clean up the empty atom and any tab still
          // referencing it (the user may have navigated to a base view via
          // main nav, leaving an orphan pill pointing at the deleted atom).
          useAtomsStore.getState().deleteAtom(atom.id).catch(console.error);
          useUIStore.getState().removeAtomFromTabs(atom.id);
        } else if (hasDraftChanges) {
          savingPromiseRef.current
            .catch(() => {})
            .then(async () => {
              const latestContent = editContentRef.current;
              const latestSourceUrl = editSourceUrlRef.current;
              const latestTags = editTagsRef.current;
              const tagIds = latestTags.map(t => t.id);
              await useAtomsStore.getState().updateAtomContentOnly(
                atom.id,
                latestContent,
                latestSourceUrl || undefined,
                tagIds,
              );
            })
            .catch(console.error);
        }
      }
    };
  }, [atom.id, readOnly]);

  // Fade save status back to idle after 2s
  useEffect(() => {
    if (saveStatus === 'saved') {
      const timer = setTimeout(() => setSaveStatus('idle'), 2000);
      return () => clearTimeout(timer);
    }
  }, [saveStatus]);

  return {
    isEditing,
    isTransitioning,
    editContent,
    editSourceUrl,
    editTags,
    saveStatus,
    cursorOffset,
    editorRevision,
    startEditing,
    stopEditing,
    setEditContent: handleSetContent,
    setEditSourceUrl: handleSetSourceUrl,
    setEditTags: handleSetTags,
    saveNow,
    flushDraft,
  };
}
