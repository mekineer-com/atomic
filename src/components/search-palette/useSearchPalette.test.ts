import { describe, expect, it } from 'vitest';
import type { GlobalSearchResponse } from '../command-palette/types';
import { nextFullSearchLimit } from './useSearchPalette';

const results = (atomCount: number, wikiCount: number): GlobalSearchResponse => ({
  atoms: Array(atomCount).fill({}),
  wiki: Array(wikiCount).fill({}),
  chats: [],
  tags: [],
}) as GlobalSearchResponse;

describe('nextFullSearchLimit', () => {
  it('continues only when an enabled section filled the current limit', () => {
    expect(nextFullSearchLimit(results(20, 2), ['atoms', 'wiki'], 20)).toBe(40);
    expect(nextFullSearchLimit(results(20, 2), ['wiki'], 20)).toBeNull();
    expect(nextFullSearchLimit(results(19, 2), ['atoms', 'wiki'], 20)).toBeNull();
    expect(nextFullSearchLimit(results(20, 2), ['atoms'], 20, 'offline')).toBeNull();
  });
});
