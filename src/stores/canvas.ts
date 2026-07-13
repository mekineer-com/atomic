import { create } from 'zustand';
import type { GlobalCanvasData } from '../lib/api';

export interface CanvasCameraState {
  x: number;
  y: number;
  ratio: number;
  angle: number;
}

export interface CanvasController {
  zoomToCluster: (label: string) => void;
  focusAtom: (atomId: string) => void;
  getCameraState: () => CanvasCameraState | null;
}

interface CanvasStore {
  // Main canvas controller — owned by the full SigmaCanvas view, driven by the
  // chat agent's tool calls (zoom_to_cluster, focus_atom).
  controller: CanvasController | null;
  registerController: (ctrl: CanvasController) => void;
  unregisterController: () => void;

  // Preview canvas controller — owned by whichever <SigmaCanvas mode="preview" />
  // is currently mounted (e.g. the dashboard briefing widget). Distinct from the
  // main controller so chat agent actions never accidentally drive a thumbnail.
  previewController: CanvasController | null;
  registerPreviewController: (ctrl: CanvasController) => void;
  unregisterPreviewController: () => void;

  // Canvas data (clusters for chat context)
  canvasData: GlobalCanvasData | null;
  canvasDataDbId: string | null;
  canvasRebuildData: GlobalCanvasData | null;
  canvasRebuildKey: string | null;
  setCanvasData: (data: GlobalCanvasData, dbId: string | null) => void;
  setCanvasRebuildData: (data: GlobalCanvasData, key: string) => void;
  invalidateCanvasData: () => void;

  // Camera state to apply to the next-mounted main canvas. Set when the user
  // clicks the dashboard preview so the main view opens at the same framing
  // they were already looking at. Consumed (cleared) on apply.
  pendingCamera: CanvasCameraState | null;
  setPendingCamera: (state: CanvasCameraState | null) => void;

  // Atom to focus + pin on the next-mounted main canvas. Set when the user
  // clicks a node in the briefing mini-canvas so the main view opens with
  // that atom selected. Consumed (cleared) on apply.
  pendingFocusAtomId: string | null;
  setPendingFocusAtomId: (id: string | null) => void;
}

export const useCanvasStore = create<CanvasStore>()((set) => ({
  controller: null,
  previewController: null,
  canvasData: null,
  canvasDataDbId: null,
  canvasRebuildData: null,
  canvasRebuildKey: null,
  pendingCamera: null,
  pendingFocusAtomId: null,

  registerController: (ctrl) => set({ controller: ctrl }),
  unregisterController: () => set({ controller: null }),

  registerPreviewController: (ctrl) => set({ previewController: ctrl }),
  unregisterPreviewController: () => set({ previewController: null }),

  setCanvasData: (data, dbId) => set({ canvasData: data, canvasDataDbId: dbId }),
  setCanvasRebuildData: (data, key) => set({ canvasRebuildData: data, canvasRebuildKey: key }),
  invalidateCanvasData: () => set({
    canvasData: null,
    canvasDataDbId: null,
    canvasRebuildData: null,
    canvasRebuildKey: null,
  }),

  setPendingCamera: (state) => set({ pendingCamera: state }),
  setPendingFocusAtomId: (id) => set({ pendingFocusAtomId: id }),
}));
