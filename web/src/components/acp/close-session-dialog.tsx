import { useEffect, useRef } from "react";

export function CloseSessionDialog({ disabled, onCancel, onConfirm }: {
  disabled: boolean;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  const keep = useRef<HTMLButtonElement>(null);
  const close = useRef<HTMLButtonElement>(null);
  useEffect(() => {
    const previous = document.activeElement;
    keep.current?.focus();
    return () => { if (previous instanceof HTMLElement && previous.isConnected) previous.focus(); };
  }, []);
  return (
    <div className="new-thread-overlay" onMouseDown={(event) => {
      if (event.target === event.currentTarget) onCancel();
    }}>
      <section className="new-thread-dialog" role="alertdialog" aria-modal="true"
        aria-labelledby="close-session-title" aria-describedby="close-session-effects"
        onKeyDown={(event) => {
          if (event.key === "Escape") { event.preventDefault(); event.stopPropagation(); onCancel(); }
          if (event.key === "Tab") {
            event.preventDefault();
            if (!disabled && document.activeElement === keep.current) close.current?.focus();
            else keep.current?.focus();
          }
        }}>
        <header><div><h2 id="close-session-title">Close session?</h2></div></header>
        <div id="close-session-effects" className="close-session-effects">
          <p>This asks the Agent to stop this session's tasks and release its resources. Managed terminal commands and their child processes may stop, including development servers.</p>
          <p>Temporary messages and terminal output will be cleared. History recovery depends on the Agent. Detached background services may keep running.</p>
        </div>
        <footer>
          <button ref={keep} type="button" className="secondary" onClick={onCancel}>Keep session</button>
          <button ref={close} type="button" className="danger" disabled={disabled} onClick={onConfirm}>Close session</button>
        </footer>
      </section>
    </div>
  );
}
