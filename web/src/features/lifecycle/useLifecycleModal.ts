import { create } from 'zustand';

export type SessionModalAction =
  | { kind: 'rename'; sessionId: string }
  | { kind: 'change-workspace'; sessionId: string }
  | { kind: 'suspend'; sessionId: string }
  | { kind: 'container-settings'; sessionId: string }
  | { kind: 'destroy'; sessionId: string };

interface LifecycleModalState {
  modal: SessionModalAction | null;
  openModal: (modal: SessionModalAction) => void;
  closeModal: () => void;
}

export const useLifecycleModal = create<LifecycleModalState>(set => ({
  modal: null,
  openModal: modal => set({ modal }),
  closeModal: () => set({ modal: null }),
}));
