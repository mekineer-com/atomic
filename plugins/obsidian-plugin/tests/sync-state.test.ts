import { describe, it, expect } from "vitest";
import { SyncState, hashContent } from "../src/sync-state";

describe("SyncState", () => {
  it("round-trips via toJSON/fromJSON", () => {
    const s = new SyncState();
    s.setFile("a.md", { atomId: "1", contentHash: "h1", lastSynced: 100 });
    s.setFile("b.md", { atomId: "2", contentHash: "h2", lastSynced: 200 });

    const json = s.toJSON();
    const restored = SyncState.fromJSON(json);

    expect(restored.getFile("a.md")).toEqual({ atomId: "1", contentHash: "h1", lastSynced: 100 });
    expect(restored.getFile("b.md")?.atomId).toBe("2");
    expect(restored.getAllPaths().sort()).toEqual(["a.md", "b.md"]);
  });

  it("handles empty/malformed input gracefully", () => {
    const s = SyncState.fromJSON({ files: {} });
    expect(s.getAllPaths()).toEqual([]);
    expect(s.getFile("missing.md")).toBeUndefined();

    // Missing 'files' key — constructor with undefined falls back to empty.
    const s2 = new SyncState();
    expect(s2.getAllPaths()).toEqual([]);
  });

  it("setFile / getFile / removeFile work", () => {
    const s = new SyncState();
    s.setFile("x.md", { atomId: "a", contentHash: "h", lastSynced: 1 });
    expect(s.getFile("x.md")?.atomId).toBe("a");
    s.removeFile("x.md");
    expect(s.getFile("x.md")).toBeUndefined();
  });

  it("renameFile moves entry", () => {
    const s = new SyncState();
    s.setFile("old.md", { atomId: "a", contentHash: "h", lastSynced: 1 });
    s.renameFile("old.md", "new.md");
    expect(s.getFile("old.md")).toBeUndefined();
    expect(s.getFile("new.md")?.atomId).toBe("a");
  });

  it("renameFile on missing path is a no-op", () => {
    const s = new SyncState();
    expect(() => s.renameFile("nope.md", "still.md")).not.toThrow();
    expect(s.getFile("still.md")).toBeUndefined();
  });

  it("clear wipes all entries", () => {
    const s = new SyncState();
    s.setFile("a.md", { atomId: "1", contentHash: "h", lastSynced: 0 });
    s.clear();
    expect(s.getAllPaths()).toEqual([]);
  });
});

describe("hashContent", () => {
  it("produces 64-char hex SHA-256", async () => {
    const h = await hashContent("hello");
    expect(h).toMatch(/^[0-9a-f]{64}$/);
  });

});
