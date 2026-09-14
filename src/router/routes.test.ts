import { describe, expect, it } from 'vitest';
import { parseLocation, toolPath } from './routes';

describe('tool routes', () => {
  it('round-trips approvals and entities', () => {
    for (const tool of ['approvals', 'entities'] as const) {
      const path = toolPath(tool);
      expect(parseLocation(path, '')).toEqual({ kind: 'tool', tool });
    }
  });
});
