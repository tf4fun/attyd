// Fits the page to the visible part of the screen. A phone's on-screen
// keyboard shrinks only the visual viewport on some browsers (iOS Safari), so
// the shell takes its height and offset from there instead of the layout
// viewport. While the keyboard covers much of the screen during composing,
// `data-keyboard` lets the stylesheet give the space to the thread.
export function initializeViewport(): () => void {
  const viewport = window.visualViewport;
  if (!viewport) return () => {};
  const root = document.documentElement;
  // The tallest visible height seen at each width: the screen without a
  // keyboard, for the current orientation.
  const fullHeights = new Map<number, number>();
  let frame = 0;
  const apply = () => {
    frame = 0;
    // Pinch zoom also shrinks the visual viewport; the layout keeps its size.
    if (viewport.scale > 1.01) return;
    const width = Math.round(viewport.width);
    const height = Math.round(viewport.height);
    const full = Math.max(fullHeights.get(width) ?? 0, height);
    fullHeights.set(width, full);
    root.style.setProperty("--viewport-height", `${height}px`);
    root.style.setProperty("--viewport-top", `${Math.max(0, Math.round(viewport.offsetTop))}px`);
    const composing = document.activeElement instanceof HTMLTextAreaElement &&
      document.activeElement.closest(".composer") != null;
    if (composing && width <= 760 && height < full * 0.75) root.dataset.keyboard = "open";
    else delete root.dataset.keyboard;
  };
  const schedule = () => {
    if (frame === 0) frame = requestAnimationFrame(apply);
  };
  apply();
  viewport.addEventListener("resize", schedule);
  viewport.addEventListener("scroll", schedule);
  document.addEventListener("focusin", schedule);
  document.addEventListener("focusout", schedule);
  return () => {
    if (frame !== 0) cancelAnimationFrame(frame);
    viewport.removeEventListener("resize", schedule);
    viewport.removeEventListener("scroll", schedule);
    document.removeEventListener("focusin", schedule);
    document.removeEventListener("focusout", schedule);
    root.style.removeProperty("--viewport-height");
    root.style.removeProperty("--viewport-top");
    delete root.dataset.keyboard;
  };
}
