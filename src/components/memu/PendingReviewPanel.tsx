import { useCallback, useEffect, useRef, useState } from 'react';
import { getTransport } from '../../lib/transport';
import { useCanvasStore } from '../../stores/canvas';
import type { AtomWithTags, DossierUsage, MemoryCitation } from '../../stores/atoms';
import { formatDate } from '../../lib/date';
import { DossierMarkdown, DossierUsageLinks, MemoryCitationLinks } from './DossierMarkdown';

type MemoryReview = {
  id: string;
  summary: string;
  memory_type: string;
  category_names?: string[];
  dossier_usages?: DossierUsage[];
  similar_to?: string[];
  similarity?: number;
};

const CLUSTER_COLORS = [
  { bar: '#f59e0b', badgeBg: 'rgba(245, 158, 11, 0.2)', badgeText: '#fcd34d' },
  { bar: '#0ea5e9', badgeBg: 'rgba(14, 165, 233, 0.2)', badgeText: '#7dd3fc' },
];

/** Assign each clustered item a color index once per fetch; removals never re-deal colors. */
function assignClusterColors(items: MemoryReview[]): Record<string, number> {
  const colorByKey: Record<string, number> = {};
  const colorByItem: Record<string, number> = {};
  let next = 0;
  for (const item of items) {
    if (!item.similar_to?.length) continue;
    const clusterKey = [item.id, ...item.similar_to].sort()[0];
    if (!(clusterKey in colorByKey)) colorByKey[clusterKey] = next++ % CLUSTER_COLORS.length;
    colorByItem[item.id] = colorByKey[clusterKey];
  }
  return colorByItem;
}

type CategoryReview = {
  id: string;
  summary: string;
  approved_summary?: string | null;
  label?: string;
  description?: string | null;
  approved_description?: string | null;
  category_kind?: 'lore' | 'topic' | 'goal' | null;
  active?: boolean;
  last_evidence_at?: string | null;
  last_revised_at?: string | null;
  citations?: MemoryCitation[];
};

type SoulSummaryReview = CategoryReview & {
  kind: string;
};

type SummaryMutationResponse = CategoryReview & {
  kind?: string;
  summaries_revision: number;
};

type PendingReviews = {
  items: MemoryReview[];
  categories: CategoryReview[];
  soul_summaries: SoulSummaryReview[];
  summaries_revision: number;
};

type SummaryScrollPosition = { approved: number; draft: number };

export function PendingReviewPanel({ onStale }: { onStale?: () => void } = {}) {
  const [reviews, setReviews] = useState<PendingReviews>({ items: [], categories: [], soul_summaries: [], summaries_revision: 0 });
  const [clusterColors, setClusterColors] = useState<Record<string, number>>({});
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [loadFailed, setLoadFailed] = useState(false);
  const [summariesStale, setSummariesStale] = useState(false);
  const [summaryBusy, setSummaryBusy] = useState(false);
  const busyReview = useRef<string | null>(null);
  const dirtyReviews = useRef(new Set<string>());
  const summariesRevision = useRef(0);
  const reviewEvents = useRef(0);
  const stale = useRef(false);
  const summaryScrollPositions = useRef(new Map<string, SummaryScrollPosition>());
  const onStaleRef = useRef(onStale);
  onStaleRef.current = onStale;
  const markStale = useCallback(() => {
    stale.current = true;
    setSummariesStale(true);
    onStaleRef.current?.();
  }, []);
  const changeSummaryBusy = useCallback((busy: boolean, reviewKey: string) => {
    busyReview.current = busy ? reviewKey : null;
    setSummaryBusy(busy);
  }, []);
  const changeSummaryDirty = useCallback((reviewKey: string, dirty: boolean) => {
    if (dirty) dirtyReviews.current.add(reviewKey);
    else dirtyReviews.current.delete(reviewKey);
  }, []);
  const scrollPosition = useCallback((id: string) => {
    let position = summaryScrollPositions.current.get(id);
    if (!position) {
      position = { approved: 0, draft: 0 };
      summaryScrollPositions.current.set(id, position);
    }
    return position;
  }, []);

  const loadReviews = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      for (let attempt = 0; attempt < 2; attempt += 1) {
        const eventVersion = reviewEvents.current;
        const fetched = await getTransport().invoke<PendingReviews>('list_pending_memu_reviews');
        if (stale.current) return;
        if (eventVersion !== reviewEvents.current || fetched.summaries_revision < summariesRevision.current) {
          if (attempt === 0) continue;
          setError('Reviews changed while loading. Refresh to try again.');
          setLoadFailed(true);
          return;
        }
        summariesRevision.current = fetched.summaries_revision;
        setReviews(fetched);
        setClusterColors(assignClusterColors(fetched.items));
        break;
      }
      setLoadFailed(false);
      setSummariesStale(false);
    } catch (err) {
      setError(String(err));
      setLoadFailed(true);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void loadReviews();
  }, [loadReviews]);

  summariesRevision.current = reviews.summaries_revision;
  useEffect(() => getTransport().subscribe<{
    category_id: string;
    summaries_revision: number;
    pending: boolean;
  }>('memu-reviews-changed', (change) => {
    if (stale.current) return;
    reviewEvents.current += 1;
    if (change.summaries_revision <= summariesRevision.current) return;
    const reviewKey = `category:${change.category_id}`;
    if (dirtyReviews.current.has(reviewKey) && busyReview.current !== reviewKey) {
      markStale();
      return;
    }
    summariesRevision.current = change.summaries_revision;
    setReviews((current) => {
      return {
        ...current,
        summaries_revision: change.summaries_revision,
        categories: change.pending
          ? current.categories
          : current.categories.filter((category) => category.id !== change.category_id),
      };
    });
  }), [markStale]);

  useEffect(() => getTransport().subscribe<SummaryMutationResponse>('memu-soul-summary-changed', (change) => {
    if (stale.current) return;
    reviewEvents.current += 1;
    if (!Number.isFinite(change.summaries_revision)) {
      void loadReviews();
      return;
    }
    if (change.summaries_revision <= summariesRevision.current || !change.kind) return;
    const reviewKey = `soul:${change.kind}`;
    if (dirtyReviews.current.has(reviewKey) && busyReview.current !== reviewKey) {
      markStale();
      return;
    }
    summariesRevision.current = change.summaries_revision;
    setReviews((current) => ({
      ...current,
      summaries_revision: change.summaries_revision,
      soul_summaries: current.soul_summaries.map((row) => row.kind === change.kind ? { ...row, ...change } : row),
    }));
  }), [loadReviews, markStale]);

  useEffect(() => getTransport().subscribe<AtomWithTags>('atom-updated', (atom) => {
    if (stale.current || !atom.id.startsWith('memory:')) return;
    const id = atom.id;
    const reviewKey = id;
    if (dirtyReviews.current.has(reviewKey) && busyReview.current !== reviewKey) {
      markStale();
      return;
    }
    if (busyReview.current === reviewKey) return;
    setReviews((current) => ({
      ...current,
      items: atom.approved_at
        ? current.items.filter((item) => item.id !== id)
        : current.items.map((item) => item.id === id ? { ...item, summary: atom.content } : item),
    }));
  }), [markStale]);

  const removeCategory = (id: string) => setReviews((r) => ({ ...r, categories: r.categories.filter((cat) => cat.id !== id) }));
  const acceptRevision = (revision: number) => {
    if (Number.isFinite(revision)) summariesRevision.current = Math.max(summariesRevision.current, revision);
  };
  const summaryActionsDisabled = loading || loadFailed || summariesStale || summaryBusy;
  // Remove the acted-on row in place (no refetch: reordering would scatter its cluster
  // mates). Survivors keep their badge/color even when the last cluster mate goes —
  // consistent visuals beat live-updating cluster membership mid-review.
  const removeMemory = (id: string) => {
    useCanvasStore.getState().invalidateCanvasData();
    setReviews((r) => ({ ...r, items: r.items.filter((item) => item.id !== id) }));
  };
  const reportError = (err: unknown) => setError(String(err));

  return (
    <div className="h-full overflow-y-auto bg-[var(--color-bg-main)] p-4">
        <div className="mb-4 flex items-start justify-between gap-3">
          <div>
            <h2 className="text-lg font-semibold text-[var(--color-text-primary)]">memU review</h2>
            <p className="text-sm text-[var(--color-text-secondary)]">Approve agent edits and pending memories.</p>
          </div>
          {!summariesStale && <button type="button" disabled={loading} onClick={() => void loadReviews()} className="rounded border border-[var(--color-border)] px-3 py-1.5 text-sm disabled:opacity-50">Refresh</button>}
        </div>

        {loading && <p className="text-sm text-[var(--color-text-secondary)]">Loading...</p>}
        {error && <p className="mb-3 rounded border border-red-500/40 bg-red-500/10 p-2 text-sm text-red-500">{error}</p>}
        {summariesStale && <p className="sticky top-0 z-20 mb-3 rounded border border-amber-500/40 bg-[var(--color-bg-main)] p-2 text-sm text-amber-500">Memory summaries changed. This tab is now a read-only snapshot. Open Approvals again to see the latest.</p>}
        {!loading && reviews.items.length === 0 && reviews.categories.length === 0 && reviews.soul_summaries.length === 0 && (
          <p className="text-sm text-[var(--color-text-secondary)]">Nothing pending.</p>
        )}

        <div
          className="grid gap-4 lg:grid-cols-[2fr_3fr]"
          onClickCapture={summariesStale ? (event) => {
            if ((event.target as HTMLElement).closest('button, a')) {
              event.preventDefault();
              event.stopPropagation();
            }
          } : undefined}
        >
          <section className="space-y-3">
            <h3 className="font-medium text-[var(--color-text-primary)]">Memories</h3>
            {reviews.items.map((item) => (
              <MemoryRow
                key={item.id}
                item={item}
                accentClass={item.similar_to?.length && clusterColors[item.id] != null ? CLUSTER_COLORS[clusterColors[item.id]] : null}
                disabled={summariesStale}
                onStale={markStale}
                onDirtyChange={changeSummaryDirty}
                onBusyChange={changeSummaryBusy}
                onDone={() => removeMemory(item.id)}
                onError={reportError}
              />
            ))}
          </section>

          <section className="space-y-3">
            <h3 className="font-medium text-[var(--color-text-primary)]">Categories</h3>
            {reviews.categories.map((category) => (
              <GeneratedSummaryRow
                key={category.id}
                review={category}
                kind="category"
                revision={reviews.summaries_revision}
                scrollPosition={scrollPosition(category.id)}
                disabled={summaryActionsDisabled}
                onStale={markStale}
                onDirtyChange={changeSummaryDirty}
                onBusyChange={changeSummaryBusy}
                onDone={(result) => {
                  acceptRevision(result.summaries_revision);
                  useCanvasStore.getState().invalidateCanvasData();
                  removeCategory(category.id);
                  setReviews((r) => ({ ...r, summaries_revision: Math.max(r.summaries_revision, result.summaries_revision) }));
                }}
                onError={reportError}
              />
            ))}
            {reviews.soul_summaries.map((summary) => (
              <GeneratedSummaryRow
                key={summary.id}
                review={summary}
                kind="soul"
                revision={reviews.summaries_revision}
                scrollPosition={scrollPosition(summary.id)}
                disabled={summaryActionsDisabled}
                onStale={markStale}
                onDirtyChange={changeSummaryDirty}
                onBusyChange={changeSummaryBusy}
                onDone={(result) => {
                  acceptRevision(result.summaries_revision);
                  setReviews((r) => ({
                    ...r,
                    summaries_revision: Number.isFinite(result.summaries_revision)
                      ? Math.max(r.summaries_revision, result.summaries_revision)
                      : r.summaries_revision,
                    soul_summaries: r.soul_summaries.map((row) => row.kind === summary.kind ? { ...row, ...result, kind: summary.kind } : row),
                  }));
                }}
                onError={reportError}
              />
            ))}
          </section>
        </div>
    </div>
  );
}

function MemoryRow({
  item,
  accentClass,
  disabled,
  onStale,
  onDirtyChange,
  onBusyChange,
  onDone,
  onError,
}: {
  item: MemoryReview;
  accentClass: (typeof CLUSTER_COLORS)[number] | null;
  disabled: boolean;
  onStale: () => void;
  onDirtyChange: (id: string, dirty: boolean) => void;
  onBusyChange: (busy: boolean, reviewKey: string) => void;
  onDone: () => void;
  onError: (err: unknown) => void;
}) {
  const [summary, setSummary] = useState(item.summary);
  const [busy, setBusy] = useState(false);
  const edited = summary !== item.summary;
  const cited = item.dossier_usages?.some(usage => usage.cited) ?? false;
  const reviewKey = item.id;
  useEffect(() => setSummary(item.summary), [item.summary]);
  useEffect(() => {
    onDirtyChange(reviewKey, edited);
    return () => onDirtyChange(reviewKey, false);
  }, [edited, onDirtyChange, reviewKey]);
  const run = async (fn: () => Promise<unknown>) => {
    setBusy(true);
    onBusyChange(true, reviewKey);
    try {
      await fn();
      await onDone();
    } catch (err) {
      if (String(err).includes('summary_snapshot_stale')) onStale();
      else onError(err);
    } finally {
      onBusyChange(false, reviewKey);
      setBusy(false);
    }
  };

  return (
    <article
      className="rounded-lg border border-[var(--color-border)] bg-[var(--color-bg-card)] p-3"
      style={accentClass ? { borderLeft: `4px solid ${accentClass.bar}` } : undefined}
    >
      <div className="mb-2 flex items-center gap-2 text-xs text-[var(--color-text-tertiary)]">
        <span className="rounded-full border border-[var(--color-border)] px-2 py-0.5">{item.memory_type}</span>
        <span>{item.category_names?.join(', ')}</span>
        {item.similarity != null && (
          <span
            className="rounded-full px-2 py-0.5 font-medium"
            style={accentClass ? { backgroundColor: accentClass.badgeBg, color: accentClass.badgeText } : undefined}
          >
            ≈ {Math.round(item.similarity * 100)}%
          </span>
        )}
      </div>
      <textarea readOnly={disabled} className="min-h-28 w-full rounded border border-[var(--color-border)] bg-transparent p-2 text-sm" value={summary} onChange={(e) => setSummary(e.target.value)} />
      <DossierUsageLinks usages={item.dossier_usages} />
      <div className="mt-2 flex gap-2">
        <button disabled={busy || disabled} className="rounded bg-[var(--color-accent)] px-3 py-1 text-sm text-white transition enabled:hover:brightness-110 enabled:focus-visible:outline enabled:focus-visible:outline-2 enabled:focus-visible:outline-offset-2 enabled:focus-visible:outline-[var(--color-accent)] disabled:cursor-not-allowed disabled:opacity-[0.45]" onClick={() => run(() => getTransport().invoke(edited ? 'update_memory_summary' : 'approve_memory', edited ? { id: item.id, summary, displayed_summary: item.summary } : { id: item.id, displayed_summary: item.summary }))}>{edited ? 'Save + approve' : 'Approve'}</button>
        <button disabled={busy || disabled || cited} title={cited ? 'Review current dossier citations before deleting' : undefined} className="rounded border border-red-500/50 px-3 py-1 text-sm text-red-500 transition-colors enabled:hover:border-red-500 enabled:hover:bg-red-500/10 enabled:focus-visible:outline enabled:focus-visible:outline-2 enabled:focus-visible:outline-offset-2 enabled:focus-visible:outline-red-500 disabled:cursor-not-allowed disabled:opacity-[0.45]" onClick={() => run(() => getTransport().invoke('delete_memory', { id: item.id, displayed_summary: item.summary }))}>Delete</button>
      </div>
    </article>
  );
}

function GeneratedSummaryRow({
  review,
  kind,
  revision,
  scrollPosition,
  disabled,
  onStale,
  onDirtyChange,
  onBusyChange,
  onDone,
  onError,
}: {
  review: CategoryReview | SoulSummaryReview;
  kind: 'category' | 'soul';
  revision: number;
  scrollPosition: SummaryScrollPosition;
  disabled: boolean;
  onStale: () => void;
  onDirtyChange: (id: string, dirty: boolean) => void;
  onBusyChange: (busy: boolean, reviewKey: string) => void;
  onDone: (result: SummaryMutationResponse) => void;
  onError: (err: unknown) => void;
}) {
  const [summary, setSummary] = useState(review.summary);
  const [title, setTitle] = useState(review.label ?? '');
  const [description, setDescription] = useState(review.description ?? '');
  const [busy, setBusy] = useState(false);
  const edited = summary !== review.summary
    || (kind === 'category' && title !== (review.label ?? ''))
    || (kind === 'category' && description !== (review.description ?? ''));
  useEffect(() => {
    setSummary(review.summary);
    setTitle(review.label ?? '');
    setDescription(review.description ?? '');
  }, [review]);
  useEffect(() => {
    const reviewKey = `${kind}:${kind === 'category' ? review.id : (review as SoulSummaryReview).kind}`;
    onDirtyChange(reviewKey, edited);
    return () => onDirtyChange(reviewKey, false);
  }, [edited, kind, onDirtyChange, review]);
  const run = async () => {
    if (disabled) return;
    setBusy(true);
    const reviewKey = `${kind}:${kind === 'category' ? review.id : (review as SoulSummaryReview).kind}`;
    onBusyChange(true, reviewKey);
    try {
      const command = kind === 'category'
        ? (edited ? 'update_category_summary' : 'approve_category')
        : (edited ? 'update_soul_summary' : 'approve_soul_summary');
      const target = kind === 'category' ? { id: review.id } : { kind: (review as SoulSummaryReview).kind };
      const changes = kind === 'category' ? {
        ...(summary !== review.summary ? { summary } : {}),
        ...(title !== (review.label ?? '') ? { title } : {}),
        ...(description !== (review.description ?? '') ? { description } : {}),
      } : { summary };
      const result = await getTransport().invoke<SummaryMutationResponse>(command, {
        ...target,
        ...(edited ? changes : {}),
        displayed_summary: review.summary,
        summaries_revision: revision,
      });
      onDone(result);
    } catch (err) {
      if (String(err) === 'summary_snapshot_stale') onStale();
      else onError(err);
    } finally {
      setBusy(false);
      onBusyChange(false, reviewKey);
    }
  };

  return (
    <article className="rounded-lg border border-[var(--color-border)] bg-[var(--color-bg-card)] p-3">
      {kind === 'category' ? (
        <div className="mb-3 space-y-2">
          <div className="flex flex-wrap items-center gap-2 text-xs text-[var(--color-text-tertiary)]">
            <span className="rounded-full border border-[var(--color-border)] px-2 py-0.5 capitalize">{review.category_kind ?? 'topic'}</span>
            <span>{review.active === false ? 'Inactive' : 'Active'}</span>
            {review.last_evidence_at && <span>Evidence: {formatDate(review.last_evidence_at)}</span>}
            {review.last_revised_at && <span>Revised: {formatDate(review.last_revised_at)}</span>}
          </div>
          <input readOnly={disabled} className="w-full rounded border border-[var(--color-border)] bg-transparent p-2 text-sm font-medium" value={title} onChange={(e) => setTitle(e.target.value)} />
          <div className="grid gap-2 md:grid-cols-2">
            <p className="min-h-20 whitespace-pre-wrap rounded border border-[var(--color-border)] p-2 text-sm text-[var(--color-text-secondary)]">{review.approved_description ?? ''}</p>
            <textarea readOnly={disabled} className="min-h-20 rounded border border-[var(--color-border)] bg-transparent p-2 text-sm" value={description} onChange={(e) => setDescription(e.target.value)} />
          </div>
        </div>
      ) : <div className="mb-2 text-sm font-medium">{review.label ?? review.id}</div>}
      <div className="grid gap-2 md:grid-cols-2">
        <div ref={(node) => { if (node) node.scrollTop = scrollPosition.approved; }} onScroll={(event) => { scrollPosition.approved = event.currentTarget.scrollTop; }} className={`prose prose-invert max-w-none overflow-y-auto rounded border border-[var(--color-border)] p-2 text-sm leading-5 [scrollbar-gutter:stable] text-[var(--color-text-secondary)] ${kind === 'category' ? 'min-h-[21rem]' : 'min-h-28'}`}>
          <DossierMarkdown citations={review.citations}>{review.approved_summary ?? ''}</DossierMarkdown>
        </div>
        <textarea readOnly={disabled} ref={(node) => { if (node) node.scrollTop = scrollPosition.draft; }} onScroll={(event) => { scrollPosition.draft = event.currentTarget.scrollTop; }} className={`overflow-y-scroll rounded border border-[var(--color-border)] bg-transparent p-2 font-sans text-sm leading-5 tracking-normal [scrollbar-gutter:stable] ${kind === 'category' ? 'min-h-[21rem]' : 'min-h-28'}`} value={summary} onChange={(e) => setSummary(e.target.value)} />
      </div>
      {kind === 'category' && Boolean(review.citations?.length) && (
        <div className="mt-2">
          <MemoryCitationLinks citations={review.citations} />
        </div>
      )}
      <div className="mt-2 flex gap-2">
        <button disabled={busy || disabled} className="rounded bg-[var(--color-accent)] px-3 py-1 text-sm text-white transition enabled:hover:brightness-110 enabled:focus-visible:outline enabled:focus-visible:outline-2 enabled:focus-visible:outline-offset-2 enabled:focus-visible:outline-[var(--color-accent)] disabled:cursor-not-allowed disabled:opacity-[0.45]" onClick={() => void run()}>{edited ? 'Save + approve' : 'Approve'}</button>
        {kind === 'category' && <button disabled title="Delete not implemented" className="rounded border border-[var(--color-border)] px-3 py-1 text-sm opacity-50">Delete</button>}
      </div>
    </article>
  );
}
