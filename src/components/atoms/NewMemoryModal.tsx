import { useEffect, useRef, useState } from 'react';
import { toast } from 'sonner';
import { useAtomsStore } from '../../stores/atoms';
import { useUIStore } from '../../stores/ui';
import { Modal } from '../ui/Modal';

interface NewMemoryModalProps {
  isOpen: boolean;
  onClose: () => void;
}

export function NewMemoryModal({ isOpen, onClose }: NewMemoryModalProps) {
  const [content, setContent] = useState('');
  const [saving, setSaving] = useState(false);
  const inputRef = useRef<HTMLTextAreaElement>(null);

  useEffect(() => {
    if (isOpen) inputRef.current?.focus();
  }, [isOpen]);

  const close = () => {
    if (saving) return;
    setContent('');
    onClose();
  };

  const create = async () => {
    if (!content.trim() || saving) return;
    setSaving(true);
    try {
      const atom = await useAtomsStore.getState().createAtom(content);
      useUIStore.getState().openReader(atom.id);
      setContent('');
      onClose();
    } catch (error) {
      toast.error('Failed to create memory', { description: String(error) });
    } finally {
      setSaving(false);
    }
  };

  return (
    <Modal
      isOpen={isOpen}
      onClose={close}
      title="New memory"
      confirmLabel={saving ? 'Creating...' : 'Create memory'}
      onConfirm={() => void create()}
      confirmDisabled={!content.trim() || saving}
    >
      <label htmlFor="new-memory-content" className="mb-2 block text-sm font-medium">
        Memory
      </label>
      <textarea
        id="new-memory-content"
        ref={inputRef}
        value={content}
        onChange={(event) => setContent(event.target.value)}
        placeholder="What should this soul remember?"
        rows={8}
        className="w-full resize-y rounded-md border border-[var(--color-border)] bg-[var(--color-bg-main)] px-3 py-2 text-sm text-[var(--color-text-primary)] outline-none focus:border-[var(--color-accent)]"
      />
    </Modal>
  );
}
