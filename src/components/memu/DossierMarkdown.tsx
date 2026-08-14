import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import type { MemoryCitation } from '../../stores/atoms';
import { useUIStore } from '../../stores/ui';

export function DossierMarkdown({
  children,
  citations = [],
}: {
  children: string;
  citations?: MemoryCitation[];
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
              className="cursor-pointer bg-transparent p-0 [font:inherit] text-inherit underline decoration-dotted"
              title={citation.summary}
              onClick={(event) => openReader(`memory:${citation.memory_id}`, undefined, { newTab: event.metaKey || event.ctrlKey })}
            >
              {label}
            </button>
          ) : <a href={href}>{label}</a>;
        },
      }}
    >
      {markdown}
    </ReactMarkdown>
  );
}

export function linkMemoryCitations(content: string, citations: Map<string, MemoryCitation>): string {
  return content.replace(/\[M[1-9]\d*\](?!\()/g, (ref) =>
    citations.has(ref) ? `${ref}(#memu-citation-${ref.slice(2, -1)})` : ref,
  );
}
