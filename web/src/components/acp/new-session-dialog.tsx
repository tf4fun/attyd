import { FolderGit2, X } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import type { AgentTransport } from "../../../../shared/bridge";
import { isAbsoluteWorkspacePath } from "../../../../shared/bridge";
import { useTranslation } from "../../i18n";

export interface NewSessionDialogProps {
  transport: AgentTransport;
  defaultCwd: string;
  purpose?: "project" | "session";
  disabled?: boolean;
  onCancel: () => void;
  onCreate: (cwd: string) => boolean;
}

export function NewSessionDialog({
  transport,
  defaultCwd,
  purpose = "session",
  disabled = false,
  onCancel,
  onCreate,
}: NewSessionDialogProps) {
  const { t } = useTranslation("workspace");
  const [cwd, setCwd] = useState(transport === "stdio" ? defaultCwd : "");
  const [submitted, setSubmitted] = useState(false);
  const input = useRef<HTMLInputElement>(null);
  const normalized = cwd.trim();
  const valid = normalized.length > 0 && isAbsoluteWorkspacePath(normalized);
  const isProject = purpose === "project";

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
            <h2 id="new-thread-title">{isProject ? t("newSession.projectTitle") : t("newSession.threadTitle")}</h2>
            <p>{isProject
              ? t("newSession.projectDescription")
              : t("newSession.threadDescription")}</p>
          </div>
          <button type="button" aria-label={isProject ? t("newSession.cancelProject") : t("newSession.cancelThread")} onClick={onCancel}>
            <X size={16} />
          </button>
        </header>

        <label htmlFor="new-thread-cwd">{isProject ? t("newSession.projectDirectory") : t("newSession.agentWorkspace")}</label>
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
            ? isProject ? t("newSession.invalidProjectPath") : t("newSession.invalidWorkspacePath")
            : transport === "stdio"
              ? t("newSession.localPathHelp")
              : t("newSession.remotePathHelp")}
        </p>

        <footer>
          <button type="button" className="secondary" onClick={onCancel}>{t("cancel")}</button>
          <button type="submit" className="primary" disabled={disabled}>{isProject ? t("newSession.createProject") : t("newSession.createThread")}</button>
        </footer>
      </form>
    </div>
  );
}
