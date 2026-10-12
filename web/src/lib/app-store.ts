import { useState, useSyncExternalStore } from "react";
import { appReducer, initialState, type AppAction, type AppState } from "./state";

export interface AppStore {
  getState(): AppState;
  dispatch(action: AppAction): void;
  subscribe(listener: () => void): () => void;
}

/**
 * A synchronous holder for the app reducer. Event handlers read the state an
 * earlier event produced, such as the session view revision a following delta
 * must extend, before React renders it.
 */
export function createAppStore(state: AppState = initialState): AppStore {
  const listeners = new Set<() => void>();
  return {
    getState: () => state,
    dispatch(action) {
      const next = appReducer(state, action);
      if (next === state) return;
      state = next;
      for (const listener of [...listeners]) listener();
    },
    subscribe(listener) {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
  };
}

export function useAppStore(): [AppState, AppStore] {
  const [store] = useState(createAppStore);
  const state = useSyncExternalStore(store.subscribe, store.getState);
  return [state, store];
}
