import { useEffect, useRef } from "react";
import { useTranslation } from "../../i18n";

export function SessionActionDialog({ action, sessionTitle, disabled, onCancel, onConfirm }: {
  action: "close" | "delete";
  sessionTitle?: string;
  disabled: boolean;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  const { t } = useTranslation("workspace");
  const deleting = action === "delete";
  const keep = useRef<HTMLButtonElement>(null);
  const confirm = useRef<HTMLButtonElement>(null);
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
        aria-labelledby="session-action-title" aria-describedby="session-action-effects"
        onKeyDown={(event) => {
          if (event.key === "Escape") { event.preventDefault(); event.stopPropagation(); onCancel(); }
          if (event.key === "Tab") {
            event.preventDefault();
            if (!disabled && document.activeElement === keep.current) confirm.current?.focus();
            else keep.current?.focus();
          }
        }}>
        <header><div><h2 id="session-action-title">{deleting ? t("deleteSessionDialog.title") : t("closeSession.title")}</h2></div></header>
        <div id="session-action-effects" className="session-action-effects">
          {sessionTitle ? <p className="session-action-target">{sessionTitle}</p> : null}
          <p>{deleting ? t("deleteSessionDialog.resourcesWarning") : t("closeSession.resourcesWarning")}</p>
          <p>{deleting ? t("deleteSessionDialog.historyWarning") : t("closeSession.historyWarning")}</p>
        </div>
        <footer>
          <button ref={keep} type="button" className="secondary" onClick={onCancel}>{t("closeSession.keep")}</button>
          <button ref={confirm} type="button" className="danger" disabled={disabled} onClick={onConfirm}>{deleting ? t("deleteSessionDialog.delete") : t("closeSession.close")}</button>
        </footer>
      </section>
    </div>
  );
}
