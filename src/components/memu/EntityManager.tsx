import { useCallback, useEffect, useMemo, useState, type FormEvent } from 'react';
import { Plus, Search, Users } from 'lucide-react';
import { getTransport } from '../../lib/transport';
import type { AtomWithTags, SemanticSearchResult } from '../../stores/atoms';
import { useCanvasStore } from '../../stores/canvas';
import { useUIStore } from '../../stores/ui';

interface EntityProperties {
  relationship?: string;
  aliases?: string[];
  ignored?: boolean;
}

interface EntitySummary {
  id: string;
  atom_id: string;
  name: string;
  normalized: string;
  entity_type: string;
  properties: EntityProperties;
  is_relationship: boolean;
  ignored: boolean;
  linked_memory_count: number;
  orphan: boolean;
  last_mentioned_at: string | null;
  created_at: string | null;
  updated_at: string | null;
}

interface EntityMemory {
  id: string;
  memory_ref: string | null;
  memory_type: string;
  summary: string;
  happened_at: string | null;
  created_at: string | null;
}

interface EntityDetail extends EntitySummary {
  memories: EntityMemory[];
}

interface EntityMergePreview {
  impact: {
    memory_count: number;
    category_titles: string[];
    current_triple_count: number;
    historical_triple_count: number;
    speaker_memory_count: number;
    aliases: string[];
    source_refs: string[];
  };
  conflicts: string[];
  warnings: string[];
  can_merge: boolean;
}

type SortMode = 'name' | 'links' | 'recent' | 'created';

function shortDate(value: string | null) {
  return value ? new Date(value).toLocaleDateString() : 'Never';
}

function normalizeMemoryRef(value: string) {
  const match = value.trim().match(/^(?:[Mm]([1-9]\d*)|\[[Mm]([1-9]\d*)\])$/);
  return match ? `[M${match[1] ?? match[2]}]` : null;
}

export function entitySaveCommand(isRelationship: boolean, promoting: boolean) {
  if (promoting) return 'promote_memu_relationship';
  return isRelationship ? 'update_memu_relationship' : 'update_memu_entity';
}

export function MemoryEntityControls({
  memoryId,
  entityIds = [],
  entityNames = [],
  onUpdated,
}: {
  memoryId: string;
  entityIds?: string[];
  entityNames?: string[];
  onUpdated: (atom: AtomWithTags) => void;
}) {
  const [entities, setEntities] = useState<EntitySummary[]>([]);
  const [query, setQuery] = useState('');
  const [error, setError] = useState<string | null>(null);
  const openReader = useUIStore(s => s.openReader);
  const attached = new Set(entityIds.map(id => id.replace(/^entity:/, '')));
  const matches = entities.filter(entity => !entity.ignored && !attached.has(entity.id) && (
    entity.name.toLocaleLowerCase().includes(query.trim().toLocaleLowerCase()) ||
    (entity.properties.aliases ?? []).some(alias => alias.toLocaleLowerCase().includes(query.trim().toLocaleLowerCase()))
  )).slice(0, 8);

  useEffect(() => {
    getTransport().invoke<{ entities: EntitySummary[] }>('list_memu_entities')
      .then(result => setEntities(result.entities))
      .catch(err => setError(err instanceof Error ? err.message : String(err)));
  }, []);

  const change = async (entityId: string, attach: boolean) => {
    setError(null);
    try {
      const updated = await getTransport().invoke<AtomWithTags>(
        attach ? 'attach_memu_entity' : 'detach_memu_entity',
        {
          memoryId: memoryId.replace(/^memory:/, ''),
          entityId: entityId.replace(/^entity:/, ''),
        },
      );
      onUpdated(updated);
      if (attach) setQuery('');
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  };

  return (
    <section className="mt-4 border-t border-[var(--color-border)] pt-4">
      <h3 className="mb-2 text-xs font-medium text-[var(--color-text-secondary)]">Entities</h3>
      <div className="mb-2 flex flex-wrap gap-1.5">
        {entityIds.map((id, index) => (
          <span key={id} className="flex items-center gap-1 rounded-full border border-[var(--color-border)] px-2 py-1 text-xs">
            <button type="button" onClick={() => openReader(id)}>{entityNames[index] ?? 'Entity'}</button>
            <button type="button" aria-label={`Detach ${entityNames[index] ?? 'entity'}`} onClick={() => void change(id, false)} className="text-[var(--color-text-tertiary)] hover:text-red-400">×</button>
          </span>
        ))}
        {entityIds.length === 0 && <span className="text-xs text-[var(--color-text-tertiary)]">None</span>}
      </div>
      <input
        value={query}
        onChange={event => setQuery(event.target.value)}
        placeholder="Find an entity to attach"
        className="w-full rounded border border-[var(--color-border)] bg-[var(--color-bg-primary)] px-2 py-1.5 text-xs outline-none focus:border-[var(--color-accent)]"
      />
      {query.trim() && (
        <div className="mt-1 space-y-1">
          {matches.map(entity => (
            <button key={entity.id} type="button" onClick={() => void change(entity.id, true)} className="block w-full rounded px-2 py-1 text-left text-xs hover:bg-[var(--color-bg-hover)]">
              {entity.name} <span className="text-[var(--color-text-tertiary)]">{entity.entity_type}</span>
            </button>
          ))}
          {matches.length === 0 && <p className="px-2 py-1 text-xs text-[var(--color-text-tertiary)]">No matches.</p>}
        </div>
      )}
      {error && <p className="mt-2 text-xs text-red-400">{error}</p>}
    </section>
  );
}

export function EntityReader({ entityId, onChanged, onDeleted }: { entityId: string; onChanged?: () => void; onDeleted?: () => void }) {
  const [entity, setEntity] = useState<EntityDetail | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [editing, setEditing] = useState(false);
  const [promoting, setPromoting] = useState(false);
  const [saving, setSaving] = useState(false);
  const [name, setName] = useState('');
  const [entityType, setEntityType] = useState('person');
  const [aliasesText, setAliasesText] = useState('');
  const [relationship, setRelationship] = useState('');
  const [memoryQuery, setMemoryQuery] = useState('');
  const [memoryResults, setMemoryResults] = useState<SemanticSearchResult[]>([]);
  const [linkedMemoryResult, setLinkedMemoryResult] = useState<EntityMemory | null>(null);
  const [mergeOpen, setMergeOpen] = useState(false);
  const [mergeCandidates, setMergeCandidates] = useState<EntitySummary[]>([]);
  const [duplicateId, setDuplicateId] = useState('');
  const [mergePreview, setMergePreview] = useState<EntityMergePreview | null>(null);
  const openReader = useUIStore(s => s.openReader);
  const removeAtomFromTabs = useUIStore(s => s.removeAtomFromTabs);

  const display = useCallback((value: EntityDetail) => {
    setEntity(value);
    setName(value.name);
    setEntityType(value.entity_type);
    setAliasesText((value.properties.aliases ?? []).join(', '));
    setRelationship(value.properties.relationship ?? '');
    setPromoting(false);
  }, []);
  const load = useCallback(async () => {
    return getTransport().invoke<EntityDetail>('get_memu_entity', { id: entityId });
  }, [entityId]);

  useEffect(() => {
    let cancelled = false;
    setEntity(null);
    setError(null);
    setMemoryQuery('');
    setMemoryResults([]);
    setLinkedMemoryResult(null);
    load()
      .then(value => { if (!cancelled) display(value); })
      .catch(err => { if (!cancelled) setError(err instanceof Error ? err.message : String(err)); });
    return () => { cancelled = true; };
  }, [display, load]);

  const refresh = async () => {
    display(await load());
    onChanged?.();
  };

  const save = async () => {
    if (!entity) return;
    setSaving(true);
    setError(null);
    const aliases = aliasesText.split(',').map(alias => alias.trim()).filter(Boolean);
    try {
      const command = entitySaveCommand(entity.is_relationship, promoting);
      if (command !== 'update_memu_entity') {
        await getTransport().invoke(command, {
          id: entity.id,
          name,
          entityType,
          aliases,
          relationship,
        });
      } else {
        await getTransport().invoke('update_memu_entity', { id: entity.id, name, entityType, aliases });
      }
      await refresh();
      setEditing(false);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setSaving(false);
    }
  };

  const removeRelationship = async () => {
    if (!entity) return;
    setSaving(true);
    setError(null);
    try {
      await getTransport().invoke('remove_memu_relationship', { id: entity.id });
      await refresh();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setSaving(false);
    }
  };

  const setIgnored = async (ignored: boolean) => {
    if (!entity) return;
    setSaving(true);
    setError(null);
    try {
      display(await getTransport().invoke<EntityDetail>(
        ignored ? 'ignore_memu_entity' : 'restore_memu_entity', { id: entity.id },
      ));
      useCanvasStore.getState().invalidateCanvasData();
      onChanged?.();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setSaving(false);
    }
  };

  const deleteEntity = async () => {
    if (!entity || !window.confirm(`Delete ${entity.name}? It may be created again later.`)) return;
    setSaving(true);
    setError(null);
    try {
      await getTransport().invoke('delete_memu_entity', { id: entity.id });
      removeAtomFromTabs(`entity:${entity.id}`);
      useCanvasStore.getState().invalidateCanvasData();
      onDeleted?.();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setSaving(false);
    }
  };

  const beginPromotion = () => {
    setPromoting(true);
    setEditing(true);
  };

  const openMerge = async () => {
    setError(null);
    try {
      const result = await getTransport().invoke<{ entities: EntitySummary[] }>('list_memu_entities');
      const candidates = result.entities.filter(candidate => candidate.id !== entityId && candidate.ignored === entity?.ignored);
      setMergeCandidates(candidates);
      setDuplicateId(candidates[0]?.id ?? '');
      setMergePreview(null);
      setMergeOpen(true);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  };

  const previewMerge = async () => {
    if (!duplicateId) return;
    setError(null);
    try {
      setMergePreview(await getTransport().invoke<EntityMergePreview>(
        'preview_memu_entity_merge', { id: entityId, duplicateId },
      ));
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  };

  const mergeEntity = async () => {
    if (!duplicateId) return;
    setSaving(true);
    setError(null);
    try {
      const merged = await getTransport().invoke<EntityDetail>(
        'merge_memu_entities', { id: entityId, duplicateId },
      );
      removeAtomFromTabs(`entity:${duplicateId}`);
      useCanvasStore.getState().invalidateCanvasData();
      display(merged);
      setMergeOpen(false);
      onChanged?.();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setSaving(false);
    }
  };

  const setMemoryEntity = async (memoryId: string, attach: boolean) => {
    setError(null);
    try {
      await getTransport().invoke(attach ? 'attach_memu_entity' : 'detach_memu_entity', {
        memoryId: memoryId.replace(/^memory:/, ''),
        entityId,
      });
      await refresh();
      setMemoryResults([]);
      setLinkedMemoryResult(null);
      setMemoryQuery('');
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  };

  const searchMemories = async (event: FormEvent) => {
    event.preventDefault();
    if (!memoryQuery.trim()) return;
    const exactRef = normalizeMemoryRef(memoryQuery);
    const linkedExact = exactRef ? entity?.memories.find(memory => memory.memory_ref === exactRef) : null;
    if (linkedExact) {
      setMemoryResults([]);
      setLinkedMemoryResult(linkedExact);
      return;
    }
    try {
      setLinkedMemoryResult(null);
      const results = await getTransport().invoke<SemanticSearchResult[]>('search_atoms_hybrid', {
        query: memoryQuery,
        limit: 8,
        memoryOnly: true,
        excludeEntityId: entityId,
      });
      setMemoryResults(results);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  };

  if (error && !entity) return <div className="p-6 text-sm text-red-400">{error}</div>;
  if (!entity) return <div className="p-6 text-sm text-[var(--color-text-tertiary)]">Loading entity...</div>;

  const aliases = entity.properties.aliases ?? [];
  const toggleEdit = () => {
    if (editing) display(entity);
    setEditing(value => !value);
  };
  return (
    <article className="h-full overflow-y-auto p-5 md:p-8">
      <div className="mx-auto max-w-3xl">
        <div className="mb-6 flex flex-wrap items-start justify-between gap-3">
          <div>
            <h1 className="text-2xl font-semibold text-[var(--color-text-primary)]">{entity.name}</h1>
            <p className="mt-1 text-sm text-[var(--color-text-tertiary)]">{entity.entity_type}</p>
          </div>
          <div className="flex flex-wrap gap-2 text-xs text-[var(--color-text-secondary)]">
            {entity.is_relationship && <span className="rounded-full border border-[var(--color-border)] px-2 py-1">Relationship</span>}
            {entity.orphan && <span className="rounded-full border border-[var(--color-border)] px-2 py-1">Orphan</span>}
            {entity.ignored && <span className="rounded-full border border-[var(--color-border)] px-2 py-1">Ignored</span>}
            <button type="button" onClick={toggleEdit} className="rounded border border-[var(--color-border)] px-2 py-1 hover:bg-[var(--color-bg-hover)]">{editing ? 'Cancel' : 'Edit'}</button>
            <button type="button" onClick={() => void openMerge()} className="rounded border border-[var(--color-border)] px-2 py-1 hover:bg-[var(--color-bg-hover)]">Merge duplicate</button>
            {!entity.is_relationship && <button type="button" disabled={saving} onClick={() => void setIgnored(!entity.ignored)} className="rounded border border-[var(--color-border)] px-2 py-1 hover:bg-[var(--color-bg-hover)] disabled:opacity-50">{entity.ignored ? 'Restore' : 'Ignore'}</button>}
            {!entity.is_relationship && entity.orphan && <button type="button" disabled={saving} onClick={() => void deleteEntity()} className="rounded border border-red-500/50 px-2 py-1 text-red-400 hover:bg-red-500/10 disabled:opacity-50">Delete</button>}
            {!entity.ignored && <button
              type="button"
              disabled={saving}
              onClick={() => entity.is_relationship ? void removeRelationship() : beginPromotion()}
              className="rounded border border-[var(--color-border)] px-2 py-1 hover:bg-[var(--color-bg-hover)] disabled:opacity-50"
            >{entity.is_relationship ? 'Remove relationship' : 'Make relationship'}</button>}
          </div>
        </div>

        {mergeOpen && (
          <section className="mb-6 space-y-3 rounded-lg border border-[var(--color-border)] bg-[var(--color-bg-card)] p-4 text-sm">
            <div className="flex items-center justify-between gap-3">
              <h2 className="font-semibold">Keep {entity.name}, merge another entity into it</h2>
              <button type="button" onClick={() => setMergeOpen(false)} className="text-[var(--color-text-tertiary)]">×</button>
            </div>
            <select value={duplicateId} onChange={event => { setDuplicateId(event.target.value); setMergePreview(null); }} className="w-full rounded border border-[var(--color-border)] bg-[var(--color-bg-primary)] p-2">
              {mergeCandidates.map(candidate => <option key={candidate.id} value={candidate.id}>{candidate.name} ({candidate.entity_type})</option>)}
            </select>
            <button type="button" disabled={!duplicateId} onClick={() => void previewMerge()} className="rounded border border-[var(--color-border)] px-3 py-1.5 hover:bg-[var(--color-bg-hover)] disabled:opacity-50">Preview merge</button>
            {mergePreview && (
              <div className="space-y-2 rounded border border-[var(--color-border)] p-3">
                <p>{mergePreview.impact.memory_count} linked memories, {mergePreview.impact.speaker_memory_count} speaker memories, {mergePreview.impact.current_triple_count} current and {mergePreview.impact.historical_triple_count} historical edges.</p>
                {mergePreview.impact.category_titles.length > 0 && <p>Dossiers: {mergePreview.impact.category_titles.join(', ')}</p>}
                {mergePreview.impact.aliases.length > 0 && <p>Aliases: {mergePreview.impact.aliases.join(', ')}</p>}
                {mergePreview.impact.source_refs.length > 0 && <p>Source references: {mergePreview.impact.source_refs.join(', ')}</p>}
                {mergePreview.warnings.map(warning => <p key={warning} className="text-amber-400">{warning}</p>)}
                {mergePreview.conflicts.map(conflict => <p key={conflict} className="text-red-400">{conflict}</p>)}
                <button type="button" disabled={saving || !mergePreview.can_merge} onClick={() => void mergeEntity()} className="rounded bg-red-600 px-3 py-1.5 text-white disabled:opacity-50">{saving ? 'Merging...' : `Keep ${entity.name} and merge selected`}</button>
              </div>
            )}
          </section>
        )}

        {editing ? (
          <section className="mb-6 space-y-3 rounded-lg border border-[var(--color-border)] bg-[var(--color-bg-card)] p-4">
            <label className="block text-xs text-[var(--color-text-tertiary)]">Name<input value={name} maxLength={entity.is_relationship || promoting ? 50 : undefined} onChange={event => setName(event.target.value)} className="mt-1 block w-full rounded border border-[var(--color-border)] bg-[var(--color-bg-primary)] p-2 text-sm text-[var(--color-text-primary)]" /></label>
            <label className="block text-xs text-[var(--color-text-tertiary)]">Type<input value={entityType} onChange={event => setEntityType(event.target.value)} placeholder="person" className="mt-1 block w-full rounded border border-[var(--color-border)] bg-[var(--color-bg-primary)] p-2 text-sm text-[var(--color-text-primary)]" /></label>
            <label className="block text-xs text-[var(--color-text-tertiary)]">Aliases<input value={aliasesText} onChange={event => setAliasesText(event.target.value)} placeholder="Comma separated" className="mt-1 block w-full rounded border border-[var(--color-border)] bg-[var(--color-bg-primary)] p-2 text-sm text-[var(--color-text-primary)]" /></label>
            {(entity.is_relationship || promoting) && <label className="block text-xs text-[var(--color-text-tertiary)]">Relationship<textarea value={relationship} maxLength={50} onChange={event => setRelationship(event.target.value)} className="mt-1 min-h-24 w-full resize-y rounded border border-[var(--color-border)] bg-[var(--color-bg-primary)] p-2 text-sm text-[var(--color-text-primary)]" /></label>}
            <button type="button" disabled={saving || !name.trim() || !entityType.trim()} onClick={() => void save()} className="rounded bg-[var(--color-accent)] px-3 py-1.5 text-sm text-white disabled:opacity-50">{saving ? 'Saving...' : promoting ? 'Make relationship' : 'Save'}</button>
          </section>
        ) : entity.is_relationship && entity.properties.relationship ? (
          <section className="mb-6 rounded-lg border border-[var(--color-border)] bg-[var(--color-bg-card)] p-4">
            <h2 className="mb-2 text-xs font-semibold uppercase tracking-wide text-[var(--color-text-tertiary)]">Relationship</h2>
            <p className="leading-relaxed text-[var(--color-text-primary)]">{entity.properties.relationship}</p>
          </section>
        ) : null}

        {error && <p className="mb-4 rounded border border-red-500/40 bg-red-500/10 p-2 text-sm text-red-400">{error}</p>}

        <dl className="mb-7 grid grid-cols-2 gap-3 text-sm md:grid-cols-4">
          <div><dt className="text-[var(--color-text-tertiary)]">Memories</dt><dd>{entity.linked_memory_count}</dd></div>
          <div><dt className="text-[var(--color-text-tertiary)]">Last mentioned</dt><dd>{shortDate(entity.last_mentioned_at)}</dd></div>
          <div><dt className="text-[var(--color-text-tertiary)]">Created</dt><dd>{shortDate(entity.created_at)}</dd></div>
          <div><dt className="text-[var(--color-text-tertiary)]">Aliases</dt><dd>{aliases.length ? aliases.join(', ') : 'None'}</dd></div>
        </dl>

        <h2 className="mb-3 text-lg font-semibold text-[var(--color-text-primary)]">Linked memories</h2>
        {entity.memories.length === 0 ? (
          <p className="text-sm text-[var(--color-text-tertiary)]">No linked memories.</p>
        ) : (
          <div className="space-y-2">
            {entity.memories.map(memory => (
              <div key={memory.id} className="flex rounded-lg border border-[var(--color-border)] bg-[var(--color-bg-card)] hover:bg-[var(--color-bg-hover)]">
                <button
                  type="button"
                  className="min-w-0 flex-1 p-3 text-left"
                  onClick={event => openReader(memory.id, undefined, {
                    newTab: event.metaKey || event.ctrlKey,
                    background: event.metaKey || event.ctrlKey,
                  })}
                >
                  <div className="mb-1 flex gap-2 text-xs text-[var(--color-text-tertiary)]">
                    {memory.memory_ref && <span>{memory.memory_ref}</span>}
                    <span>{memory.memory_type}</span>
                    <span>{shortDate(memory.happened_at ?? memory.created_at)}</span>
                  </div>
                  <p className="text-sm leading-relaxed text-[var(--color-text-primary)]">{memory.summary}</p>
                </button>
                <button type="button" aria-label="Detach memory" onClick={() => void setMemoryEntity(memory.id, false)} className="px-3 text-[var(--color-text-tertiary)] hover:text-red-400">×</button>
              </div>
            ))}
          </div>
        )}
        {!entity.ignored && <form onSubmit={searchMemories} className="mt-4 flex gap-2">
          <input value={memoryQuery} onChange={event => setMemoryQuery(event.target.value)} placeholder="Find a memory to attach" className="min-w-0 flex-1 rounded border border-[var(--color-border)] bg-[var(--color-bg-primary)] px-3 py-2 text-sm outline-none focus:border-[var(--color-accent)]" />
          <button type="submit" className="rounded border border-[var(--color-border)] px-3 py-2 text-sm hover:bg-[var(--color-bg-hover)]">Search</button>
        </form>}
        {!entity.ignored && memoryResults.length > 0 && <div className="mt-2 space-y-1">{memoryResults.map(memory => (
          <div key={memory.id} title={memory.content} className="flex items-center gap-2 rounded px-3 py-2 text-sm hover:bg-[var(--color-bg-hover)]">
            <span className="min-w-0 flex-1">{memory.title || memory.snippet}</span>
            <button type="button" onClick={() => openReader(memory.id)} className="text-[var(--color-text-tertiary)] hover:text-[var(--color-text-primary)]">Open</button>
            <button type="button" onClick={() => void setMemoryEntity(memory.id, true)} className="text-[var(--color-accent)]">Attach</button>
          </div>
        ))}</div>}
        {!entity.ignored && linkedMemoryResult && <button type="button" title={linkedMemoryResult.summary} onClick={() => openReader(linkedMemoryResult.id)} className="mt-2 block w-full rounded px-3 py-2 text-left text-sm hover:bg-[var(--color-bg-hover)]"><span className="mr-2 text-[var(--color-text-tertiary)]">{linkedMemoryResult.memory_ref} · Already linked</span>{linkedMemoryResult.summary}</button>}
      </div>
    </article>
  );
}

export function EntityManager() {
  const [entities, setEntities] = useState<EntitySummary[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [query, setQuery] = useState('');
  const [type, setType] = useState('all');
  const [relationshipOnly, setRelationshipOnly] = useState(false);
  const [showIgnored, setShowIgnored] = useState(false);
  const [orphanOnly, setOrphanOnly] = useState(false);
  const [sort, setSort] = useState<SortMode>('name');
  const [error, setError] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const [creatingEntity, setCreatingEntity] = useState(false);
  const [newName, setNewName] = useState('');
  const [newType, setNewType] = useState('person');

  const loadEntities = useCallback(async () => {
    const result = await getTransport().invoke<{ entities: EntitySummary[] }>('list_memu_entities');
    setEntities(result.entities);
    setSelectedId(current => current ?? result.entities.find(entity => !entity.ignored)?.id ?? null);
  }, []);

  useEffect(() => {
    loadEntities().catch(err => setError(err instanceof Error ? err.message : String(err)));
  }, [loadEntities]);

  const createEntity = async (event: FormEvent) => {
    event.preventDefault();
    if (!newName.trim() || !newType.trim()) return;
    setCreatingEntity(true);
    setError(null);
    try {
      const created = await getTransport().invoke<EntitySummary>('create_memu_entity', {
        name: newName.trim(),
        entityType: newType,
      });
      await loadEntities();
      setSelectedId(created.id);
      setNewName('');
      setNewType('person');
      setCreating(false);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setCreatingEntity(false);
    }
  };

  const visible = useMemo(() => {
    const needle = query.trim().toLocaleLowerCase();
    return entities
      .filter(entity => !needle || entity.name.toLocaleLowerCase().includes(needle) ||
        (entity.properties.aliases ?? []).some(alias => alias.toLocaleLowerCase().includes(needle)))
      .filter(entity => type === 'all' || entity.entity_type.toLocaleLowerCase() === type)
      .filter(entity => !relationshipOnly || entity.is_relationship)
      .filter(entity => showIgnored || !entity.ignored)
      .filter(entity => !orphanOnly || entity.orphan)
      .sort((left, right) => {
        if (sort === 'links') return right.linked_memory_count - left.linked_memory_count || left.name.localeCompare(right.name);
        if (sort === 'recent') return (right.last_mentioned_at ?? '').localeCompare(left.last_mentioned_at ?? '') || left.name.localeCompare(right.name);
        if (sort === 'created') return (right.created_at ?? '').localeCompare(left.created_at ?? '') || left.name.localeCompare(right.name);
        return left.name.localeCompare(right.name);
      });
  }, [entities, orphanOnly, query, relationshipOnly, showIgnored, sort, type]);
  const entityTypes = useMemo(() => Array.from(
    new Map(entities.map(entity => [entity.entity_type.toLocaleLowerCase(), entity.entity_type])).values(),
  ).sort((left, right) => left.localeCompare(right)), [entities]);

  if (error && entities.length === 0) return <div className="p-6 text-sm text-red-400">{error}</div>;

  return (
    <div className="grid h-full overflow-y-auto md:grid-cols-[20rem_minmax(0,1fr)] md:overflow-hidden">
      <aside className="border-b border-[var(--color-border)] bg-[var(--color-bg-secondary)] md:overflow-y-auto md:border-b-0 md:border-r">
        <div className="sticky top-0 z-10 border-b border-[var(--color-border)] bg-[var(--color-bg-secondary)] p-4">
          <div className="mb-3 flex items-center gap-2">
            <Users className="h-5 w-5" />
            <h1 className="font-semibold">Entities</h1>
            <button type="button" onClick={() => setCreating(value => !value)} className="ml-auto flex items-center gap-1 rounded border border-[var(--color-border)] px-2 py-1 text-xs hover:bg-[var(--color-bg-hover)]"><Plus className="h-3.5 w-3.5" />Add</button>
            <span className="text-xs text-[var(--color-text-tertiary)]">{visible.length}</span>
          </div>
          {creating && <form onSubmit={createEntity} className="mb-3 space-y-2 rounded border border-[var(--color-border)] bg-[var(--color-bg-card)] p-2">
            <input autoFocus value={newName} onChange={event => setNewName(event.target.value)} placeholder="Entity name" className="w-full rounded border border-[var(--color-border)] bg-[var(--color-bg-primary)] px-2 py-1.5 text-xs outline-none focus:border-[var(--color-accent)]" />
            <div className="flex gap-2">
              <input value={newType} onChange={event => setNewType(event.target.value)} placeholder="Type" className="min-w-0 flex-1 rounded border border-[var(--color-border)] bg-[var(--color-bg-primary)] p-1.5 text-xs" />
              <button type="submit" disabled={creatingEntity || !newName.trim() || !newType.trim()} className="rounded bg-[var(--color-accent)] px-2 py-1 text-xs text-white disabled:opacity-50">{creatingEntity ? 'Adding...' : 'Create'}</button>
            </div>
          </form>}
          {error && <p className="mb-3 text-xs text-red-400">{error}</p>}
          <label className="mb-3 flex items-center gap-2 rounded-md border border-[var(--color-border)] bg-[var(--color-bg-primary)] px-2">
            <Search className="h-4 w-4 text-[var(--color-text-tertiary)]" />
            <input
              value={query}
              onChange={event => setQuery(event.target.value)}
              placeholder="Search names and aliases"
              className="min-w-0 flex-1 bg-transparent py-2 text-sm outline-none"
            />
          </label>
          <div className="grid grid-cols-2 gap-2">
            <select value={type} onChange={event => setType(event.target.value)} className="rounded border border-[var(--color-border)] bg-[var(--color-bg-primary)] p-1.5 text-xs">
              <option value="all">All types</option>{entityTypes.map(entityType => <option key={entityType} value={entityType.toLocaleLowerCase()}>{entityType}</option>)}
            </select>
            <select value={sort} onChange={event => setSort(event.target.value as SortMode)} className="rounded border border-[var(--color-border)] bg-[var(--color-bg-primary)] p-1.5 text-xs">
              <option value="name">Name</option><option value="links">Most memories</option><option value="recent">Recently mentioned</option><option value="created">Recently created</option>
            </select>
          </div>
          <div className="mt-3 flex flex-wrap gap-x-3 gap-y-1 text-xs text-[var(--color-text-secondary)]">
            <label><input type="checkbox" checked={relationshipOnly} onChange={event => setRelationshipOnly(event.target.checked)} className="mr-1" />Relationships</label>
            <label><input type="checkbox" checked={showIgnored} onChange={event => setShowIgnored(event.target.checked)} className="mr-1" />Show ignored ({entities.filter(entity => entity.ignored).length})</label>
            <label><input type="checkbox" checked={orphanOnly} onChange={event => setOrphanOnly(event.target.checked)} className="mr-1" />Orphans</label>
          </div>
        </div>
        <div className="p-2">
          {visible.map(entity => (
            <button
              key={entity.id}
              type="button"
              onClick={() => setSelectedId(entity.id)}
              className={`mb-1 w-full rounded-md px-3 py-2 text-left ${selectedId === entity.id ? 'bg-[var(--color-bg-hover)] text-[var(--color-text-primary)]' : 'text-[var(--color-text-secondary)] hover:bg-[var(--color-bg-hover)]'}`}
            >
              <div className="truncate text-sm font-medium">{entity.name}</div>
              <div className="mt-0.5 flex gap-2 text-xs text-[var(--color-text-tertiary)]"><span>{entity.entity_type}</span><span>{entity.linked_memory_count} memories</span></div>
            </button>
          ))}
          {visible.length === 0 && <p className="p-4 text-center text-sm text-[var(--color-text-tertiary)]">No matching entities.</p>}
        </div>
      </aside>
      <main className="min-h-[30rem] md:min-h-0 md:overflow-hidden">
        {selectedId ? <EntityReader entityId={selectedId} onChanged={() => void loadEntities()} onDeleted={() => { setSelectedId(null); void loadEntities(); }} /> : <div className="p-6 text-sm text-[var(--color-text-tertiary)]">Select an entity.</div>}
      </main>
    </div>
  );
}
