import { ArrowLeft, Bot } from "lucide-react";
import type { ReactNode } from "react";
import { useTranslation } from "../../i18n";

export function SessionOpening({ error, backLabel, onBack, onRetry, children }: {
  error?: string;
  backLabel: string;
  onBack: () => void;
  onRetry: () => void;
  children?: ReactNode;
}) {
  const { t } = useTranslation("app");
  return <div className="session-opening-panel">
    <header className="session-header">
      <div className="page-navigation">
        <button type="button" className="page-back" aria-label={backLabel} title={backLabel} onClick={onBack}>
          <ArrowLeft size={18} aria-hidden="true" />
        </button>
        <span className="workspace-breadcrumbs">{backLabel}</span>
      </div>
      <div className="session-header-main">
        <div className="thread-heading">
          <div className="thread-agent-icon" aria-hidden="true"><Bot size={21} /></div>
          <div role={error == null ? "status" : undefined}>
            <h1>{t(error == null ? "sessionOpening.loading" : "sessionOpening.failed")}</h1>
            {error == null ? <span>{t("sessionOpening.description")}</span> : null}
          </div>
        </div>
        {error == null ? <div className="session-header-actions" aria-hidden="true">
          <Skeleton className="skeleton-control" /><Skeleton className="skeleton-control" />
        </div> : null}
      </div>
    </header>
    <div className="thread-body">
      <div className="scroll-region">
        <div className="conversation-wrap" aria-busy={error == null}>
          {children}
          {error == null ? <div className="conversation session-skeleton" aria-hidden="true">
            {[0, 1].map((turn) => <div className="conversation-turn" key={turn}>
              <div className="message message-user skeleton-message">
                <Skeleton className="skeleton-role" />
                <Skeleton className="skeleton-question" />
              </div>
              <div className="skeleton-process"><Skeleton className="skeleton-role" /><Skeleton className="skeleton-count" /></div>
              <div className="message message-agent skeleton-message">
                <Skeleton className="skeleton-role" />
                <Skeleton /><Skeleton /><Skeleton className="skeleton-last-line" />
              </div>
            </div>)}
          </div> : <div className="session-opening-error">
            <p role="alert">{error}</p>
            <button type="button" className="session-opening-retry" onClick={onRetry}>{t("sessionOpening.retry")}</button>
          </div>}
        </div>
      </div>
    </div>
    {error == null ? <div className="input-dock" aria-hidden="true">
      <div className="input-inner">
        <div className="composer skeleton-composer">
          <Skeleton className="skeleton-question" />
          <div className="skeleton-composer-bar"><Skeleton className="skeleton-role" /><Skeleton className="skeleton-control" /></div>
        </div>
      </div>
    </div> : null}
  </div>;
}

function Skeleton({ className = "" }: { className?: string }) {
  return <div className={`skeleton ${className}`} />;
}
