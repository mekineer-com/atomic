import { describe, expect, it } from 'vitest';
import { shouldOpenChatSearch } from './ChatView';

describe('shouldOpenChatSearch', () => {
  it('leaves browser find to hidden chat and memU surfaces', () => {
    expect(shouldOpenChatSearch(false)).toBe(false);
    expect(shouldOpenChatSearch(true, { type: 'tool', tool: 'approvals' })).toBe(false);
    expect(shouldOpenChatSearch(true, {
      type: 'atom', atomId: 'memory:1', tagId: null, highlightText: null, editing: false,
    })).toBe(false);
  });

  it('keeps chat search on ordinary workspace surfaces', () => {
    expect(shouldOpenChatSearch(true, {
      type: 'atom', atomId: 'workspace-note', tagId: null, highlightText: null, editing: false,
    })).toBe(true);
  });
});
