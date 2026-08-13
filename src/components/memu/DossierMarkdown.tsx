import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import type { MemoryCitation } from '../../stores/atoms';

export function DossierMarkdown({
  children,
  citations = [],
}: {
  children: string;
  citations?: MemoryCitation[];
}) {
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
            <span className="cursor-help underline decoration-dotted" title={citation.summary}>{label}</span>
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
