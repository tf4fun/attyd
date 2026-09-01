import { useEffect, useRef, type RefObject } from "react";

const DEFAULT_FOCUS_TARGET =
  "input:not([disabled]), select:not([disabled]), textarea:not([disabled]), button:not([disabled]), a[href]";

/**
 * Moves keyboard focus into a transient Agent interaction and restores the
 * previously focused control when that interaction is removed.
 *
 * Multiple interaction cards can mount at once. Each card remembers the
 * element that was focused immediately before it, so resolving them unwinds
 * focus in the same order instead of always jumping to the composer.
 */
export function useInteractionFocus<T extends HTMLElement>(
  selector = DEFAULT_FOCUS_TARGET,
): RefObject<T | null> {
  const container = useRef<T>(null);

  useEffect(() => {
    const previous = document.activeElement instanceof HTMLElement
      ? document.activeElement
      : undefined;
    container.current?.querySelector<HTMLElement>(selector)?.focus();

    return () => {
      if (
        !previous ||
        previous === document.body ||
        !previous.isConnected
      ) return;
      const active = document.activeElement;
      if (
        active == null ||
        active === document.body ||
        container.current?.contains(active)
      ) {
        previous.focus();
      }
    };
  }, [selector]);

  return container;
}
