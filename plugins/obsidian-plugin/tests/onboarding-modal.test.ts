import { describe, it, expect, vi } from "vitest";
import { App, requestUrl } from "obsidian";
import { OnboardingModal } from "../src/onboarding-modal";
import { DEFAULT_SETTINGS } from "../src/settings";
import { AtomicClient } from "../src/atomic-client";

const mockRequestUrl = requestUrl as unknown as ReturnType<typeof vi.fn>;

function makePlugin(app: App) {
  const settings = { ...DEFAULT_SETTINGS, authToken: "t" };
  return {
    app,
    settings,
    saveSettings: vi.fn(async () => {}),
    client: new AtomicClient(settings),
    syncEngine: {
      startWatching: vi.fn(),
      stopWatching: vi.fn(),
      syncAll: vi.fn(async () => ({
        phase: "complete",
        totalFiles: 0,
        processed: 0,
        created: 0,
        updated: 0,
        skipped: 0,
        errors: 0,
        atomIds: [],
      })),
      shouldExclude: () => false,
    },
    ws: { open: vi.fn(), on: vi.fn(() => () => {}) },
  } as any;
}

describe("OnboardingModal", () => {
  it("transitions Welcome -> Connect when 'Get Started' is clicked", () => {
    const app = new App();
    const plugin = makePlugin(app);
    const modal = new OnboardingModal(app, plugin);
    modal.onOpen();
    expect(modal.contentEl.querySelector("h2")?.textContent).toMatch(/Welcome/);
    const btn = Array.from(modal.contentEl.querySelectorAll("button")).find(
      (b) => b.textContent === "Get started"
    );
    expect(btn).toBeTruthy();
    btn!.click();
    expect(modal.contentEl.querySelector("h2")?.textContent).toMatch(/Connect/);
  });

  it("disables Next button until connection verified, enables on success", async () => {
    const app = new App();
    const plugin = makePlugin(app);
    const modal = new OnboardingModal(app, plugin);
    (modal as any).currentStep = 1;
    modal.onOpen();

    const getNext = () =>
      Array.from(modal.contentEl.querySelectorAll("button")).find(
        (b) => b.textContent === "Next"
      ) as HTMLButtonElement | undefined;
    const getTest = () =>
      Array.from(modal.contentEl.querySelectorAll("button")).find(
        (b) => b.textContent === "Test connection"
      ) as HTMLButtonElement | undefined;

    expect(getNext()?.disabled).toBe(true);

    mockRequestUrl.mockResolvedValue({
      status: 200,
      json: {},
      text: "",
      headers: {},
      arrayBuffer: new ArrayBuffer(0),
    });
    getTest()!.click();
    // The click handler is async — flush microtasks + dynamic import.
    await new Promise((r) => setTimeout(r, 10));
    await new Promise((r) => setTimeout(r, 10));
    expect(getNext()?.disabled).toBe(false);
  });

  it("shows error status on failed connection test", async () => {
    const app = new App();
    const plugin = makePlugin(app);
    const modal = new OnboardingModal(app, plugin);
    (modal as any).currentStep = 1;
    modal.onOpen();

    mockRequestUrl.mockResolvedValue({
      status: 401,
      json: { error: "bad token" },
      text: "",
      headers: {},
      arrayBuffer: new ArrayBuffer(0),
    });
    const testBtn = Array.from(modal.contentEl.querySelectorAll("button")).find(
      (b) => b.textContent === "Test connection"
    ) as HTMLButtonElement;
    testBtn.click();
    await new Promise((r) => setTimeout(r, 20));
    const status = modal.contentEl.querySelector(".atomic-onboarding-test-status");
    expect(status?.classList.contains("error")).toBe(true);
    expect(status?.textContent).toMatch(/bad token/);
  });
});
