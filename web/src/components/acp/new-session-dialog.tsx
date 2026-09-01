import { FolderGit2, X } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import type { AgentTransport } from "../../../../shared/bridge";
import { isAbsoluteWorkspacePath } from "../../../../shared/bridge";

export interface NewSessionDialogProps {
  transport: AgentTransport;
  defaultCwd: string;
  disabled?: boolean;
  onCancel: () => void;
  onCreate: (cwd: string) => boolean;
}

export function NewSessionDialog({
  transport,
  defaultCwd,
  disabled = false,
  onCancel,
  onCreate,
}: NewSessionDialogProps) {
  const [cwd, setCwd] = useState(transport === "stdio" ? defaultCwd : "");
  const [submitted, setSubmitted] = useState(false);
  const input = useRef<HTMLInputElement>(null);
  const normalized = cwd.trim();
  const valid = normalized.length > 0 && isAbsoluteWorkspacePath(normalized);

  useEffect(() => {
    input.current?.focus();
    input.current?.select();
    const closeOnEscape = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      event.preventDefault();
      onCancel();
    };
    window.addEventListener("keydown", closeOnEscape);
    return () => window.removeEventListener("keydown", closeOnEscape);
  }, [onCancel]);

  return (
    <div
      className="new-thread-overlay"
      onMouseDown={(event) => {
        if (event.target === event.currentTarget) onCancel();
      }}
    >
      <form
        className="new-thread-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="new-thread-title"
        onSubmit={(event) => {
          event.preventDefault();
          setSubmitted(true);
          if (!valid || disabled) return;
          onCreate(normalized);
        }}
      >
        <header>
          <span className="new-thread-dialog-icon"><FolderGit2 size={17} /></span>
          <div>
            <h2 id="new-thread-title">New thread</h2>
            <p>Choose the workspace this Agent session can work in.</p>
          </div>
          <button type="button" aria-label="Cancel new thread" onClick={onCancel}>
            <X size={16} />
          </button>
        </header>

        <label htmlFor="new-thread-cwd">Agent workspace</label>
        <input
          ref={input}
          id="new-thread-cwd"
          aria-describedby="new-thread-cwd-help"
          aria-invalid={submitted && !valid ? "true" : undefined}
          autoComplete="off"
          spellCheck={false}
          placeholder={transport === "stdio" ? defaultCwd : "/home/user/project"}
          value={cwd}
          onChange={(event) => {
            setCwd(event.target.value);
            setSubmitted(false);
          }}
        />
        <p
          id="new-thread-cwd-help"
          className={submitted && !valid ? "new-thread-path-error" : undefined}
        >
          {submitted && !valid
            ? "Enter an absolute workspace path."
            : transport === "stdio"
              ? "Local path on the machine running attyd."
              : "Absolute path on the remote Agent host."}
        </p>

        <footer>
          <button type="button" className="secondary" onClick={onCancel}>Cancel</button>
          <button type="submit" className="primary" disabled={disabled}>Create thread</button>
        </footer>
      </form>
    </div>
  );
}
