import { describe, expect, it } from 'vitest';

import { entitySaveCommand } from './EntityManager';

describe('entitySaveCommand', () => {
  it.each([
    [false, false, 'update_memu_entity'],
    [false, true, 'promote_memu_relationship'],
    [true, false, 'update_memu_relationship'],
    [true, true, 'promote_memu_relationship'],
  ])('routes relationship=%s promoting=%s to %s', (isRelationship, promoting, command) => {
    expect(entitySaveCommand(isRelationship, promoting)).toBe(command);
  });
});
