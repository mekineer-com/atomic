import { useEffect, useMemo, useState } from 'react';
import { Search, Users } from 'lucide-react';
import { getTransport } from '../../lib/transport';
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

type SortMode = 'name' | 'links' | 'recent' | 'created';

function shortDate(value: string | null) {
  return value ? new Date(value).toLocaleDateString() : 'Never';
}

export function EntityReader({ entityId }: { entityId: string }) {
  const [entity, setEntity] = useState<EntityDetail | null>(null);
  const [error, setError] = useState<string | null>(null);
  const openReader = useUIStore(s => s.openReader);

  useEffect(() => {
    let cancelled = false;
    setEntity(null);
    setError(null);
    getTransport().invoke<EntityDetail>('get_memu_entity', { id: entityId })
      .then(value => { if (!cancelled) setEntity(value); })
      .catch(err => { if (!cancelled) setError(err instanceof Error ? err.message : String(err)); });
    return () => { cancelled = true; };
  }, [entityId]);

  if (error) return <div className="p-6 text-sm text-red-400">{error}</div>;
  if (!entity) return <div className="p-6 text-sm text-[var(--color-text-tertiary)]">Loading entity...</div>;

  const aliases = entity.properties.aliases ?? [];
  return (
    <article className="h-full overflow-y-auto p-5 md:p-8">
      <div className="mx-auto max-w-3xl">
        <div className="mb-6 flex flex-wrap items-start justify-between gap-3">
          <div>
            <h1 className="text-2xl font-semibold text-[var(--color-text-primary)]">{entity.name}</h1>
            <p className="mt-1 text-sm capitalize text-[var(--color-text-tertiary)]">{entity.entity_type}</p>
          </div>
          <div className="flex gap-2 text-xs text-[var(--color-text-secondary)]">
            {entity.is_relationship && <span className="rounded-full border border-[var(--color-border)] px-2 py-1">Relationship</span>}
            {entity.orphan && <span className="rounded-full border border-[var(--color-border)] px-2 py-1">Orphan</span>}
            {entity.ignored && <span className="rounded-full border border-[var(--color-border)] px-2 py-1">Ignored</span>}
          </div>
        </div>

        {entity.properties.relationship && (
          <section className="mb-6 rounded-lg border border-[var(--color-border)] bg-[var(--color-bg-card)] p-4">
            <h2 className="mb-2 text-xs font-semibold uppercase tracking-wide text-[var(--color-text-tertiary)]">Relationship</h2>
            <p className="leading-relaxed text-[var(--color-text-primary)]">{entity.properties.relationship}</p>
          </section>
        )}

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
              <button
                key={memory.id}
                type="button"
                className="block w-full rounded-lg border border-[var(--color-border)] bg-[var(--color-bg-card)] p-3 text-left hover:bg-[var(--color-bg-hover)]"
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
            ))}
          </div>
        )}
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
  const [ignoredOnly, setIgnoredOnly] = useState(false);
  const [orphanOnly, setOrphanOnly] = useState(false);
  const [sort, setSort] = useState<SortMode>('name');
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    getTransport().invoke<{ entities: EntitySummary[] }>('list_memu_entities')
      .then(result => {
        setEntities(result.entities);
        setSelectedId(current => current ?? result.entities[0]?.id ?? null);
      })
      .catch(err => setError(err instanceof Error ? err.message : String(err)));
  }, []);

  const visible = useMemo(() => {
    const needle = query.trim().toLocaleLowerCase();
    return entities
      .filter(entity => !needle || entity.name.toLocaleLowerCase().includes(needle) ||
        (entity.properties.aliases ?? []).some(alias => alias.toLocaleLowerCase().includes(needle)))
      .filter(entity => type === 'all' || entity.entity_type === type)
      .filter(entity => !relationshipOnly || entity.is_relationship)
      .filter(entity => !ignoredOnly || entity.ignored)
      .filter(entity => !orphanOnly || entity.orphan)
      .sort((left, right) => {
        if (sort === 'links') return right.linked_memory_count - left.linked_memory_count || left.name.localeCompare(right.name);
        if (sort === 'recent') return (right.last_mentioned_at ?? '').localeCompare(left.last_mentioned_at ?? '') || left.name.localeCompare(right.name);
        if (sort === 'created') return (right.created_at ?? '').localeCompare(left.created_at ?? '') || left.name.localeCompare(right.name);
        return left.name.localeCompare(right.name);
      });
  }, [entities, ignoredOnly, orphanOnly, query, relationshipOnly, sort, type]);

  if (error) return <div className="p-6 text-sm text-red-400">{error}</div>;

  return (
    <div className="grid h-full overflow-y-auto md:grid-cols-[20rem_minmax(0,1fr)] md:overflow-hidden">
      <aside className="border-b border-[var(--color-border)] bg-[var(--color-bg-secondary)] md:overflow-y-auto md:border-b-0 md:border-r">
        <div className="sticky top-0 z-10 border-b border-[var(--color-border)] bg-[var(--color-bg-secondary)] p-4">
          <div className="mb-3 flex items-center gap-2">
            <Users className="h-5 w-5" />
            <h1 className="font-semibold">Entities</h1>
            <span className="ml-auto text-xs text-[var(--color-text-tertiary)]">{visible.length}</span>
          </div>
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
              <option value="all">All types</option><option value="person">People</option><option value="place">Places</option><option value="topic">Topics</option><option value="project">Projects</option>
            </select>
            <select value={sort} onChange={event => setSort(event.target.value as SortMode)} className="rounded border border-[var(--color-border)] bg-[var(--color-bg-primary)] p-1.5 text-xs">
              <option value="name">Name</option><option value="links">Most memories</option><option value="recent">Recently mentioned</option><option value="created">Recently created</option>
            </select>
          </div>
          <div className="mt-3 flex flex-wrap gap-x-3 gap-y-1 text-xs text-[var(--color-text-secondary)]">
            <label><input type="checkbox" checked={relationshipOnly} onChange={event => setRelationshipOnly(event.target.checked)} className="mr-1" />Relationships</label>
            <label><input type="checkbox" checked={ignoredOnly} onChange={event => setIgnoredOnly(event.target.checked)} className="mr-1" />Ignored</label>
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
              <div className="mt-0.5 flex gap-2 text-xs text-[var(--color-text-tertiary)]"><span className="capitalize">{entity.entity_type}</span><span>{entity.linked_memory_count} memories</span></div>
            </button>
          ))}
          {visible.length === 0 && <p className="p-4 text-center text-sm text-[var(--color-text-tertiary)]">No matching entities.</p>}
        </div>
      </aside>
      <main className="min-h-[30rem] md:min-h-0 md:overflow-hidden">
        {selectedId ? <EntityReader entityId={selectedId} /> : <div className="p-6 text-sm text-[var(--color-text-tertiary)]">Select an entity.</div>}
      </main>
    </div>
  );
}
