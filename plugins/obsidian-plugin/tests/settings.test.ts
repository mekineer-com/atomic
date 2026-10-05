import { describe, it, expect, vi } from "vitest";
import { App } from "obsidian";
import { DEFAULT_SETTINGS, AtomicSettingTab } from "../src/settings";

describe("DEFAULT_SETTINGS", () => {
  it("has sensible defaults", () => {
    expect(DEFAULT_SETTINGS.serverUrl).toBe("http://localhost:8080");
    expect(DEFAULT_SETTINGS.authToken).toBe("");
    expect(DEFAULT_SETTINGS.autoSync).toBe(true);
    expect(DEFAULT_SETTINGS.syncDebounceMs).toBe(2000);
    expect(DEFAULT_SETTINGS.excludePatterns).toContain(".git/**");
    expect(DEFAULT_SETTINGS.deleteOnRemove).toBe(false);
  });
});

describe("AtomicSettingTab.display", () => {
  it("renders without throwing", () => {
    const app = new App();
    const client = { testConnection: vi.fn(async () => {}) };
    const syncEngine = {
      startWatching: vi.fn(),
      stopWatching: vi.fn(),
      resetAndResync: vi.fn(async () => ({})),
    };
    const plugin: any = {
      app,
      settings: { ...DEFAULT_SETTINGS },
      saveSettings: vi.fn(async () => {}),
      client,
      syncEngine,
    };
    const tab = new AtomicSettingTab(app, plugin);
    expect(() => tab.display()).not.toThrow();

  });
});
