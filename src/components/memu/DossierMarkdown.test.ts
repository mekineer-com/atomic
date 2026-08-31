import { describe, expect, it } from 'vitest';
import type { MemoryCitation } from '../../stores/atoms';
import { linkMemoryCitations, safeDossierHref } from './DossierMarkdown';

describe('linkMemoryCitations', () => {
  it('links resolved references without hiding unresolved or existing links', () => {
    const citation = { ref: '[M1]', memory_id: '1', summary: 'A memory' } satisfies MemoryCitation;
    expect(linkMemoryCitations('[M1] [M2] [M1](https://example.test)', new Map([[citation.ref, citation]])))
      .toBe('[M1](#memu-citation-1) [M2] [M1](https://example.test)');
  });

  it('rejects executable links in model-authored prose', () => {
    expect(safeDossierHref('https://example.test')).toBe('https://example.test');
    expect(safeDossierHref('javascript:alert(1)')).toBeUndefined();
  });
});
