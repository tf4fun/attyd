import type { PlanEntry, SessionUpdate } from "@agentclientprotocol/sdk";
import { Check, ChevronRight, Circle, CircleDot, FileText, ListChecks, X } from "lucide-react";
import { useEffect, useId, useRef, useState } from "react";
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

  if (update.sessionUpdate === "plan" || update.sessionUpdate === "plan_update") {
    return <PlanDisclosure key={entryId} entryId={entryId} update={update} />;
  }

  return null;
}

function PlanDisclosure({
  update,
  entryId,
}: {
  update: Extract<SessionUpdate, { sessionUpdate: "plan" | "plan_update" }>;
  entryId: string;
}) {
  const { t, i18n } = useTranslation("cards");
  const [open, setOpen] = useState(true);
  const bodyId = useId();
  const disclosure = useRef<HTMLElement>(null);
  const disclosureMounted = useRef(false);
  const entries = update.sessionUpdate === "plan"
    ? update.entries
    : update.plan.type === "items" ? update.plan.entries : undefined;
  const file = update.sessionUpdate === "plan_update" && update.plan.type === "file" ? update.plan : undefined;
  const markdown = update.sessionUpdate === "plan_update" && update.plan.type === "markdown" ? update.plan : undefined;
  const Icon = file ? FileText : ListChecks;

  useEffect(() => {
    if (!disclosureMounted.current) {
      disclosureMounted.current = true;
      return;
    }
    disclosure.current?.dispatchEvent(new Event("toggle"));
  }, [open]);

  return (
    <section ref={disclosure} className="plan-card" data-thread-entry data-thread-entry-id={entryId} data-open={open ? "true" : "false"}>
      <header className="plan-heading">
        <button type="button" className="plan-disclosure" aria-expanded={open} aria-controls={bodyId} onClick={() => setOpen((value) => !value)}>
          <Icon size={15} aria-hidden="true" />
          <strong data-thread-searchable>{t(file ? "plan.file" : "plan.title")}</strong>
          {entries ? <span className="plan-progress">{t("plan.progress", {
            completed: entries.filter((entry) => entry.status === "completed").length.toLocaleString(i18n.resolvedLanguage),
            total: entries.length.toLocaleString(i18n.resolvedLanguage),
          })}</span> : null}
        </button>
        <button type="button" className="component-disclosure-button" aria-label={t(open ? "plan.collapse" : "plan.expand")} aria-expanded={open} aria-controls={bodyId} onClick={() => setOpen((value) => !value)}>
          <ChevronRight className="chevron" size={15} aria-hidden="true" />
        </button>
      </header>
      <div id={bodyId} className="plan-body" data-thread-searchable hidden={!open}>
        {entries ? <PlanEntries entries={entries} /> : null}
        {file ? <code>{file.uri}</code> : null}
        {markdown ? <div className="markdown"><Markdown remarkPlugins={[remarkGfm]}>{markdown.content}</Markdown></div> : null}
        <RawJson label={t("plan.event")} value={update} />
      </div>
    </section>
  );
}

function PlanEntries({ entries }: { entries: PlanEntry[] }) {
  const { t } = useTranslation("cards");
  return (
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
  );
}
