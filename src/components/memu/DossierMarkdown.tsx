import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import type { DossierUsage, MemoryCitation } from '../../stores/atoms';
import { useUIStore } from '../../stores/ui';

export const safeDossierHref = (href?: string) => href && /^(https?:|mailto:|#)/i.test(href) ? href : undefined;

export function DossierMarkdown({
  children,
  citations = [],
  highlightRef,
}: {
  children: string;
  citations?: MemoryCitation[];
  highlightRef?: string | null;
}) {
  const openReader = useUIStore(s => s.openReader);
  const byRef = new Map(citations.map((citation) => [citation.ref, citation]));
  const markdown = linkMemoryCitations(children, byRef);

  return (
    <ReactMarkdown
      remarkPlugins={[remarkGfm]}
      components={{
        a: ({ href, children: label }) => {
          const ref = href?.match(/^#memu-citation-(\d+)$/)?.[1];
          const citation = ref ? byRef.get(`[M${ref}]`) : undefined;
          return citation ? (
            <button
              type="button"
              autoFocus={citation.ref === highlightRef}
              className={`cursor-pointer p-0 [font:inherit] text-inherit underline decoration-dotted ${citation.ref === highlightRef ? 'rounded bg-amber-500/25' : 'bg-transparent'}`}
              title={citation.summary}
              onClick={(event) => openReader(`memory:${citation.memory_id}`, undefined, {
                newTab: event.metaKey || event.ctrlKey,
                background: event.metaKey || event.ctrlKey,
              })}
            >
              {label}
            </button>
          ) : safeDossierHref(href) ? <a href={href}>{label}</a> : <>{label}</>;
        },
      }}
    >
      {markdown}
    </ReactMarkdown>
  );
}

export function MemoryCitationLinks({ citations = [] }: { citations?: MemoryCitation[] }) {
  const openReader = useUIStore(s => s.openReader);

  if (citations.length === 0) return null;
  return (
    <div className="flex flex-wrap gap-1">
      {citations.map((citation) => (
        <button
          key={citation.ref}
          type="button"
          title={citation.summary}
          className="cursor-pointer rounded border border-[var(--color-border)] px-1.5 py-0.5 text-xs underline decoration-dotted"
          onClick={(event) => openReader(`memory:${citation.memory_id}`, undefined, {
            newTab: event.metaKey || event.ctrlKey,
            background: event.metaKey || event.ctrlKey,
          })}
        >
          {citation.ref}
        </button>
      ))}
    </div>
  );
}

export function DossierUsageLinks({ usages = [] }: { usages?: DossierUsage[] }) {
  const openReader = useUIStore(s => s.openReader);
  if (usages.length === 0) return null;
  return (
    <div className="flex flex-wrap items-center gap-1.5 text-xs text-[var(--color-text-tertiary)]">
      <span>Used by</span>
      {usages.map(usage => (
        <button
          key={usage.id}
          type="button"
          onClick={event => openReader(usage.id, usage.cited ? usage.ref ?? undefined : undefined, {
            newTab: event.metaKey || event.ctrlKey,
            background: event.metaKey || event.ctrlKey,
          })}
          className={`rounded-full border px-2 py-0.5 hover:text-[var(--color-text-primary)] ${usage.cited ? 'border-amber-500/50 text-amber-400' : 'border-[var(--color-border)]'}`}
        >
          {usage.name}{usage.cited ? ' · cited' : ''}
        </button>
      ))}
    </div>
  );
}

export function linkMemoryCitations(content: string, citations: Map<string, MemoryCitation>): string {
  return content.replace(/\[M[1-9]\d*\](?!\()/g, (ref) =>
    citations.has(ref) ? `${ref}(#memu-citation-${ref.slice(2, -1)})` : ref,
  );
}
