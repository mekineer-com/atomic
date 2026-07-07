import { Plus, Search, Tag, BookOpen, MessageCircle, LayoutGrid, List as ListIcon, Settings, RefreshCw, GitMerge, X } from 'lucide-react';
import { getTransport } from '../../lib/transport';
import { Command, CommandCategory } from './types';
import { useAtomsStore } from '../../stores/atoms';
import { useUIStore } from '../../stores/ui';
import { useTagsStore } from '../../stores/tags';

// Icon components as simple wrappers
const PlusIcon = () => <Plus className="w-4 h-4" strokeWidth={2} />;
const SearchIcon = () => <Search className="w-4 h-4" strokeWidth={2} />;
const TagIcon = () => <Tag className="w-4 h-4" strokeWidth={2} />;
const BookOpenIcon = () => <BookOpen className="w-4 h-4" strokeWidth={2} />;
const MessageCircleIcon = () => <MessageCircle className="w-4 h-4" strokeWidth={2} />;
const LayoutGridIcon = () => <LayoutGrid className="w-4 h-4" strokeWidth={2} />;
const ListIconWrapper = () => <ListIcon className="w-4 h-4" strokeWidth={2} />;
const SettingsIcon = () => <Settings className="w-4 h-4" strokeWidth={2} />;
const RefreshIcon = () => <RefreshCw className="w-4 h-4" strokeWidth={2} />;
const MergeIcon = () => <GitMerge className="w-4 h-4" strokeWidth={2} />;
const XIcon = () => <X className="w-4 h-4" strokeWidth={2} />;

// Command definitions
export const commands: Command[] = [
  // Navigation commands
  {
    id: 'open-wiki-list',
    label: 'Open wiki list',
    category: 'navigation',
    keywords: ['wiki', 'articles', 'list', 'browse', 'knowledge'],
    icon: BookOpenIcon,
    action: () => useUIStore.getState().setViewMode('wiki'),
  },
  {
    id: 'open-chat-list',
    label: 'Open chat list',
    category: 'navigation',
    keywords: ['chat', 'conversations', 'messages', 'talk'],
    icon: MessageCircleIcon,
    action: () => useUIStore.getState().openChatSidebar(),
  },
  {
    id: 'create-new-chat',
    label: 'Create new chat',
    category: 'navigation',
    keywords: ['chat', 'conversation', 'new', 'start'],
    icon: MessageCircleIcon,
    action: () => useUIStore.getState().openChatSidebar(),
  },
  {
    id: 'switch-to-grid',
    label: 'Switch to grid layout',
    category: 'navigation',
    keywords: ['view', 'grid', 'cards', 'tiles', 'atoms'],
    icon: LayoutGridIcon,
    action: () => {
      const ui = useUIStore.getState();
      ui.setViewMode('atoms');
      ui.setAtomsLayout('grid');
    },
    isEnabled: () => {
      const ui = useUIStore.getState();
      return ui.viewMode !== 'atoms' || ui.atomsLayout !== 'grid';
    },
  },
  {
    id: 'switch-to-list',
    label: 'Switch to list layout',
    category: 'navigation',
    keywords: ['view', 'list', 'rows', 'compact', 'atoms'],
    icon: ListIconWrapper,
    action: () => {
      const ui = useUIStore.getState();
      ui.setViewMode('atoms');
      ui.setAtomsLayout('list');
    },
    isEnabled: () => {
      const ui = useUIStore.getState();
      return ui.viewMode !== 'atoms' || ui.atomsLayout !== 'list';
    },
  },
  {
    id: 'open-settings',
    label: 'Open settings',
    category: 'navigation',
    keywords: ['settings', 'preferences', 'config', 'options', 'setup'],
    icon: SettingsIcon,
    action: () => {
      // Settings modal is managed separately, we'll emit a custom event
      window.dispatchEvent(new CustomEvent('open-settings'));
    },
  },

  // Atom commands
  {
    id: 'create-atom',
    label: 'Create new atom',
    category: 'atoms',
    keywords: ['new', 'add', 'write', 'note', 'create', 'atom'],
    shortcut: '⌘N',
    icon: PlusIcon,
    action: async () => {
      const { createAtom } = useAtomsStore.getState();
      const newAtom = await createAtom('');
      useUIStore.getState().openReaderEditing(newAtom.id);
    },
  },
  {
    id: 'search-atoms',
    label: 'Open search',
    category: 'atoms',
    keywords: ['search', 'find', 'query', 'semantic', 'lookup'],
    shortcut: '⌘P',
    icon: SearchIcon,
    action: () => useUIStore.getState().openSearchPalette(),
  },

  // Tag commands
  {
    id: 'filter-by-tag',
    label: 'Search tags...',
    category: 'tags',
    keywords: ['tag', 'filter', 'category', 'label'],
    shortcut: '#',
    icon: TagIcon,
    action: () => useUIStore.getState().openSearchPalette('#'),
  },
  {
    id: 'create-tag',
    label: 'Create new tag',
    category: 'tags',
    keywords: ['tag', 'new', 'add', 'create', 'category'],
    icon: PlusIcon,
    action: async () => {
      const name = window.prompt('Enter tag name:');
      if (name && name.trim()) {
        await useTagsStore.getState().createTag(name.trim());
      }
    },
  },
  {
    id: 'compact-tags',
    label: 'Compact tags (AI-assisted)',
    category: 'tags',
    keywords: ['compact', 'merge', 'clean', 'organize', 'ai', 'llm'],
    icon: MergeIcon,
    action: async () => {
      await useTagsStore.getState().compactTags();
    },
  },
  {
    id: 'clear-tag-filter',
    label: 'Clear tag filter',
    category: 'tags',
    keywords: ['clear', 'reset', 'remove', 'filter'],
    icon: XIcon,
    action: () => useUIStore.getState().setSelectedTag(null),
    isEnabled: () => {
      const ui = useUIStore.getState();
      return ui.selectedTagId !== null && ui.viewMode !== 'canvas';
    },
  },

  // Utility commands
  {
    id: 'retry-failed-embeddings',
    label: 'Retry failed embeddings',
    category: 'utility',
    keywords: ['retry', 'failed', 'embedding', 'process', 'fix'],
    icon: RefreshIcon,
    action: async () => {
      try {
        const count = await getTransport().invoke<number>('process_pending_embeddings');
        if (count > 0) {
          console.log(`Retrying ${count} pending embeddings...`);
        }
      } catch (error) {
        console.error('Failed to retry embeddings:', error);
      }
    },
  },
];

// Category labels for display
export const categoryLabels: Record<CommandCategory, string> = {
  navigation: 'Navigation',
  atoms: 'Atoms',
  tags: 'Tags',
  wiki: 'Wiki',
  utility: 'Utility',
};

// Category order for display
export const categoryOrder: CommandCategory[] = [
  'navigation',
  'atoms',
  'tags',
  'wiki',
  'utility',
];

// Get commands grouped by category
export function getGroupedCommands(): Map<CommandCategory, Command[]> {
  const grouped = new Map<CommandCategory, Command[]>();

  for (const category of categoryOrder) {
    const categoryCommands = commands.filter(
      (cmd) => cmd.category === category && (cmd.isEnabled?.() ?? true)
    );
    if (categoryCommands.length > 0) {
      grouped.set(category, categoryCommands);
    }
  }

  return grouped;
}
