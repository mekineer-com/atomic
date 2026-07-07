import { useEffect, useState } from 'react';
import { LeftPanel } from './LeftPanel';
import { MainView } from './MainView';
import { LoadingIndicator } from '../ui/LoadingIndicator';
import { ServerConnectionStatus } from '../ui/ServerConnectionStatus';
import { RouterBridge } from '../../router/RouterBridge';
import { SettingsModal } from '../settings/SettingsModal';
import { OnboardingWizard } from '../onboarding';
import { CommandPalette } from '../command-palette';
import { SearchPalette } from '../search-palette/SearchPalette';
import { useAtomsStore } from '../../stores/atoms';
import { useTagsStore } from '../../stores/tags';
import { useDatabasesStore } from '../../stores/databases';
import { useUIStore } from '../../stores/ui';
import { useTheme, useFont } from '../../hooks';
import { verifyProviderConfigured } from '../../lib/api';
import { isTauri } from '../../lib/platform';
import { hasServerConfig } from '../../lib/transport';


export function Layout() {
  useTheme(); // Initialize theme
  useFont(); // Initialize font
  const fetchAtoms = useAtomsStore(s => s.fetchAtoms);
  const fetchTags = useTagsStore(s => s.fetchTags);
  const [isSetupRequired, setIsSetupRequired] = useState<boolean | null>(null); // null = checking
  const [settingsOpen, setSettingsOpen] = useState(false);

  // Command palette state
  const commandPaletteOpen = useUIStore((state) => state.commandPaletteOpen);
  const toggleCommandPalette = useUIStore((state) => state.toggleCommandPalette);
  const closeCommandPalette = useUIStore((state) => state.closeCommandPalette);
  const searchPaletteOpen = useUIStore((state) => state.searchPaletteOpen);
  const searchPaletteInitialQuery = useUIStore((state) => state.searchPaletteInitialQuery);
  const toggleSearchPalette = useUIStore((state) => state.toggleSearchPalette);
  const closeSearchPalette = useUIStore((state) => state.closeSearchPalette);
  const openSearchPalette = useUIStore((state) => state.openSearchPalette);

  // Global keyboard shortcuts
  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      // Don't trigger shortcuts when typing in input fields (except for command palette toggle)
      const isInputActive =
        document.activeElement?.tagName === 'INPUT' ||
        document.activeElement?.tagName === 'TEXTAREA' ||
        (document.activeElement as HTMLElement)?.isContentEditable;

      // Cmd+Shift+P or Ctrl+Shift+P to toggle command palette (works even in inputs)
      if ((e.metaKey || e.ctrlKey) && e.shiftKey && e.key.toLowerCase() === 'p') {
        e.preventDefault();
        toggleCommandPalette();
        return;
      }

      // Cmd+P or Ctrl+P to toggle global search (works even in inputs)
      if ((e.metaKey || e.ctrlKey) && e.key === 'p') {
        e.preventDefault();
        toggleSearchPalette();
        return;
      }

      // Skip other shortcuts if input is active
      if (isInputActive) return;

      // "/" to open the global search palette
      if (e.key === '/' && !commandPaletteOpen && !searchPaletteOpen) {
        e.preventDefault();
        openSearchPalette();
        return;
      }

      // "#" to open the search palette in tag-only mode
      if (e.key === '#' && !commandPaletteOpen && !searchPaletteOpen) {
        e.preventDefault();
        openSearchPalette('#');
        return;
      }

      // Cmd+N or Ctrl+N to create new atom (only when palettes are closed)
      if ((e.metaKey || e.ctrlKey) && e.key === 'n' && !commandPaletteOpen && !searchPaletteOpen) {
        e.preventDefault();
        const { createAtom } = useAtomsStore.getState();
        createAtom('').then((newAtom) => {
          useUIStore.getState().openReaderEditing(newAtom.id);
        }).catch(console.error);
        return;
      }
    };

    window.addEventListener('keydown', handleKeyDown);
    return () => window.removeEventListener('keydown', handleKeyDown);
  }, [toggleCommandPalette, toggleSearchPalette, openSearchPalette, commandPaletteOpen, searchPaletteOpen]);

  // Listen for custom settings event from command palette
  useEffect(() => {
    const handleOpenSettings = () => setSettingsOpen(true);
    window.addEventListener('open-settings', handleOpenSettings);
    return () => window.removeEventListener('open-settings', handleOpenSettings);
  }, []);

  // Listen for auth expiry (stale/revoked token) and transition to setup mode
  useEffect(() => {
    const handler = () => setIsSetupRequired(true);
    window.addEventListener('atomic:auth-expired', handler);
    return () => window.removeEventListener('atomic:auth-expired', handler);
  }, []);

  // Check if setup is needed on mount
  useEffect(() => {
    let retryTimer: ReturnType<typeof setTimeout> | null = null;
    let cancelled = false;
    let startupRetries = 0;
    const maxStartupRetries = 30;

    const checkSetup = async () => {
      try {
        const configured = await verifyProviderConfigured();
        if (cancelled) return;
        startupRetries = 0;
        setIsSetupRequired(!configured);

        if (configured) {
          // Only initialize app if provider is configured
          await initializeApp();
        }
      } catch (error) {
        console.error('Failed to check provider configuration:', error);
        if (cancelled) return;
        const message = String(error);
        if (
          hasServerConfig() &&
          startupRetries < maxStartupRetries &&
          (message.includes('Failed to fetch') || message.includes('NetworkError'))
        ) {
          startupRetries++;
          retryTimer = setTimeout(checkSetup, 1000);
          return;
        }
        // If check fails, show setup anyway
        setIsSetupRequired(true);
      }
    };

    checkSetup();
    return () => {
      cancelled = true;
      if (retryTimer) clearTimeout(retryTimer);
    };
  }, []);

  const initializeApp = async () => {
    // Paint cached tag tree + atoms first (if any) so the UI has something
    // to show while the network call races. Hydration reads the persisted
    // `activeId`; it no-ops on first-ever session (no cache yet).
    //
    // `fetchDatabases` runs in parallel so `activeId` is fresh by the time
    // atoms/tags fetches finish and want to write to the cache. We don't
    // await it because we don't want cache hydration to block on a network
    // round-trip — the offline case still needs to paint instantly.
    void useDatabasesStore.getState().fetchDatabases();
    await Promise.all([
      useAtomsStore.getState().hydrateFromCache(),
      useTagsStore.getState().hydrateFromCache(),
    ]);
    await Promise.all([fetchAtoms(), fetchTags()]);
  };

  const handleSetupComplete = async () => {
    setIsSetupRequired(false);
    // Now initialize the app
    await initializeApp();
  };

  // Show loading while checking
  if (isSetupRequired === null) {
    return (
      <div className={`flex h-full items-center justify-center bg-[var(--color-bg-main)] ${isTauri() ? 'pt-[28px]' : ''}`}>
        <span className="text-[var(--color-text-secondary)]">Loading...</span>
      </div>
    );
  }

  // Show onboarding wizard if setup is required
  if (isSetupRequired) {
    return (
      <div className={`flex h-full overflow-hidden bg-[var(--color-bg-main)] ${isTauri() ? 'pt-[28px]' : ''}`}>
        <OnboardingWizard onComplete={handleSetupComplete} />
      </div>
    );
  }

  return (
    <div className="flex h-full overflow-hidden bg-[var(--color-bg-main)]">
      <RouterBridge />
      <LeftPanel />
      <MainView />
      <LoadingIndicator />
      <ServerConnectionStatus />
      <CommandPalette
        isOpen={commandPaletteOpen}
        onClose={closeCommandPalette}
      />
      <SearchPalette
        isOpen={searchPaletteOpen}
        onClose={closeSearchPalette}
        initialQuery={searchPaletteInitialQuery}
      />
      <SettingsModal
        isOpen={settingsOpen}
        onClose={() => setSettingsOpen(false)}
      />
    </div>
  );
}
