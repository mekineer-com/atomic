import { currentIdentity } from './openalma-identity';
import { useAtomsStore } from '../stores/atoms';
import { useUIStore } from '../stores/ui';

export const NEW_MEMORY_EVENT = 'atomic:new-memory';

export async function startNewAtom(): Promise<void> {
  if (currentIdentity() && useUIStore.getState().knowledgeSource !== 'workspace') {
    window.dispatchEvent(new Event(NEW_MEMORY_EVENT));
    return;
  }

  const source = useUIStore.getState().knowledgeSource;
  const atom = await useAtomsStore.getState().createAtom('');
  if (useUIStore.getState().knowledgeSource !== source) return;
  useUIStore.getState().openReaderEditing(atom.id);
}
