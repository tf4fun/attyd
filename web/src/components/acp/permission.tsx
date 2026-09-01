import type { RequestPermissionResponse, ToolCall } from "@agentclientprotocol/sdk";
import { ShieldAlert } from "lucide-react";
import type { PendingPermission } from "../../lib/state";
import { useInteractionFocus } from "../../lib/use-interaction-focus";
import { RawJson } from "./raw-json";

export function PermissionCard({
  pending,
  toolCall,
  onRespond,
}: {
  pending: PendingPermission;
  toolCall?: ToolCall;
  onRespond: (outcome: RequestPermissionResponse["outcome"]) => void;
}) {
  const { request } = pending;
  const responding = pending.responseRequestId != null;
  const title = request.toolCall.title ?? toolCall?.title ?? "The agent wants to run a tool";
  const name = request.toolCall.name ?? toolCall?.name;
  const kind = request.toolCall.kind ?? toolCall?.kind;
  const locations = request.toolCall.locations ?? toolCall?.locations;
  const rawInput = Object.prototype.hasOwnProperty.call(request.toolCall, "rawInput")
    ? request.toolCall.rawInput
    : toolCall?.rawInput;
  const inspectable = rawInput !== undefined || (locations?.length ?? 0) > 0;
  const card = useInteractionFocus<HTMLDivElement>();
  return (
    <div
      ref={card}
      className="permission-card"
      role="alertdialog"
      aria-label="Agent permission request"
      aria-busy={responding}
      onKeyDown={(event) => {
        if (event.key !== "Escape" || responding) return;
        event.preventDefault();
        event.stopPropagation();
        onRespond({ outcome: "cancelled" });
      }}
    >
      <div className="permission-title">
        <span className="permission-icon"><ShieldAlert size={17} /></span>
        <div>
          <strong>Permission required</strong>
          <p>{title}</p>
        </div>
      </div>
      <div className="permission-subject" data-testid="permission-subject">
        {name || kind ? (
          <div className="permission-tool-meta">
            {name ? <code>{name}</code> : null}
            {kind ? <span>{kind}</span> : null}
          </div>
        ) : null}
        {locations && locations.length > 0 ? (
          <div className="locations">
            {locations.map((location, index) => (
              <code key={`${location.path}:${location.line ?? ""}:${index}`}>
                {location.path}{location.line != null ? `:${location.line}` : ""}
              </code>
            ))}
          </div>
        ) : null}
        {rawInput !== undefined ? <RawJson label="Tool input" value={rawInput} open /> : null}
        {!inspectable ? (
          <p className="permission-warning">The Agent did not provide inspectable tool input or locations.</p>
        ) : null}
      </div>
      <div className="permission-actions">
        {request.options.map((option) => (
          <button
            className={`permission-${option.kind}`}
            key={option.optionId}
            disabled={responding}
            onClick={() =>
              onRespond({ outcome: "selected", optionId: option.optionId })
            }
          >
            {option.name}
          </button>
        ))}
        <button className="ghost" disabled={responding} onClick={() => onRespond({ outcome: "cancelled" })}>
          Cancel
        </button>
      </div>
      {responding ? <p className="interaction-status" role="status">Sending response…</p> : null}
      {pending.responseError ? <p className="interaction-error" role="alert">{pending.responseError}</p> : null}
      <RawJson label="Permission request" value={request} />
    </div>
  );
}
