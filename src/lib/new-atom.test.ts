import { beforeEach, describe, expect, it, vi } from 'vitest';
import { useAtomsStore } from '../stores/atoms';
import { useUIStore } from '../stores/ui';
import { NEW_MEMORY_EVENT, startNewAtom } from './new-atom';

describe('startNewAtom', () => {
  beforeEach(() => sessionStorage.clear());

  it('opens the integrated memory dialog without creating a blank atom', async () => {
    sessionStorage.setItem('openalma.user', 'TestOwner');
    sessionStorage.setItem('openalma.soul', 'TestSoul');
    const createAtom = vi.fn();
    useAtomsStore.setState({ createAtom });
    const opened = vi.fn();
    window.addEventListener(NEW_MEMORY_EVENT, opened, { once: true });

    await startNewAtom();

    expect(opened).toHaveBeenCalledOnce();
    expect(createAtom).not.toHaveBeenCalled();
  });

  it('keeps standalone blank-atom editing', async () => {
    const createAtom = vi.fn().mockResolvedValue({ id: 'atom:test' });
    const openReaderEditing = vi.fn();
    useAtomsStore.setState({ createAtom });
    useUIStore.setState({ openReaderEditing });

    await startNewAtom();

    expect(createAtom).toHaveBeenCalledWith('');
    expect(openReaderEditing).toHaveBeenCalledWith('atom:test');
  });
});
