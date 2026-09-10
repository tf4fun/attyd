import type { PlanEntry, SessionUpdate } from "@agentclientprotocol/sdk";
import { Check, Circle, CircleDot, FileText, ListChecks, X } from "lucide-react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { useTranslation } from "../../i18n";
import { RawJson } from "./raw-json";

export function PlanCard({ update, entryId }: { update: SessionUpdate; entryId: string }) {
  const { t } = useTranslation("cards");
  if (update.sessionUpdate === "plan_removed") {
    return (
      <div className="plan-card plan-removed" data-thread-entry data-thread-entry-id={entryId} data-thread-searchable>
        <X size={15} /> {t("plan.removed", { id: update.planId })}
      </div>
    );
  }

  if (update.sessionUpdate === "plan") {
    return <PlanEntries entryId={entryId} entries={update.entries} raw={update} />;
  }

  if (update.sessionUpdate === "plan_update") {
    if (update.plan.type === "items") {
      return <PlanEntries entryId={entryId} entries={update.plan.entries} raw={update} />;
    }
    if (update.plan.type === "file") {
      return (
        <div className="plan-card" data-thread-entry data-thread-entry-id={entryId} data-thread-searchable>
          <div className="plan-heading">
            <FileText size={15} /> {t("plan.file")}
          </div>
          <code>{update.plan.uri}</code>
          <RawJson label={t("plan.event")} value={update} />
        </div>
      );
    }
    return (
      <div className="plan-card" data-thread-entry data-thread-entry-id={entryId} data-thread-searchable>
        <div className="plan-heading">
          <ListChecks size={15} /> {t("plan.title")}
        </div>
        <div className="markdown">
          <Markdown remarkPlugins={[remarkGfm]}>{update.plan.content}</Markdown>
        </div>
        <RawJson label={t("plan.event")} value={update} />
      </div>
    );
  }

  return null;
}

function PlanEntries({
  entries,
  raw,
  entryId,
}: {
  entries: PlanEntry[];
  raw: unknown;
  entryId: string;
}) {
  const { t, i18n } = useTranslation("cards");
  return (
    <div className="plan-card" data-thread-entry data-thread-entry-id={entryId} data-thread-searchable>
      <div className="plan-heading">
        <ListChecks size={15} /> {t("plan.title")} <span>{t("plan.progress", { completed: entries.filter((e) => e.status === "completed").length.toLocaleString(i18n.resolvedLanguage), total: entries.length.toLocaleString(i18n.resolvedLanguage) })}</span>
      </div>
      <ol>
        {entries.map((entry, index) => (
          <li key={`${entry.content}:${index}`} className={entry.status}>
            {entry.status === "completed" ? (
              <Check size={14} />
            ) : entry.status === "in_progress" ? (
              <CircleDot size={14} />
            ) : (
              <Circle size={14} />
            )}
            <span>{entry.content}</span>
            <small>{t(`plan.priority.${entry.priority}`)}</small>
          </li>
        ))}
      </ol>
      <RawJson label={t("plan.event")} value={raw} />
    </div>
  );
}
