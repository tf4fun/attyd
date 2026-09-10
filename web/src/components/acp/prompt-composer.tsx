import type { AvailableCommand, ContentBlock, PromptCapabilities } from "@agentclientprotocol/sdk";
import {
  AtSign,
  CornerDownLeft,
  File,
  FileText,
  Image as ImageIcon,
  Link2,
  LoaderCircle,
  Maximize2,
  Minimize2,
  Music2,
  Paperclip,
  Plus,
  RefreshCw,
  Square,
  WifiOff,
  X,
} from "lucide-react";
import {
  useEffect,
  useMemo,
  useRef,
  useState,
  type ChangeEvent,
  type ClipboardEvent,
  type DragEvent,
  type KeyboardEvent,
  type ReactNode,
} from "react";
import type {
  WorkspaceContextAttachment,
  WorkspaceContextMatch,
} from "../../../../shared/bridge";
import i18n, { useTranslation } from "../../i18n";
import { randomId } from "../../lib/id";
import { MAX_ATTACHMENT_BYTES, createPromptAttachments, PromptAttachmentError, promptAttachmentErrorMessage, type PromptAttachment } from "../../lib/prompt-attachments";
import { ContextUsage, type ContextUsageValue } from "./context-usage";

export interface ComposerDraft {
  id: string;
  blocks: ContentBlock[];
}

export type ThreadNavigationTarget =
  | "top"
  | "bottom"
  | "page-up"
  | "page-down"
  | "previous-message"
  | "next-message"
  | "previous-prompt"
  | "next-prompt"
  | "latest-prompt";

export function PromptComposer({
  disabled,
  running,
  capabilities,
  commands,
  sessionControls,
  usage,
  interactionPending = false,
  connectionRecovery,
  draft,
  history = [],
  onSubmit,
  onCancel,
  onNavigateThread,
  onSearchWorkspaceContext,
  onReadWorkspaceContext,
}: {
  disabled: boolean;
  running: boolean;
  capabilities?: PromptCapabilities | null;
  commands: AvailableCommand[];
  sessionControls?: ReactNode;
  usage?: ContextUsageValue;
  interactionPending?: boolean;
  connectionRecovery?: {
    message: string;
    onReconnect: () => void;
  };
  draft?: ComposerDraft;
  history?: ContentBlock[][];
  onSubmit: (
    text: string,
    attachments: ContentBlock[],
    restoredBlocks?: ContentBlock[],
  ) => boolean | void;
  onCancel: () => void;
  onNavigateThread?: (target: ThreadNavigationTarget) => void;
  onSearchWorkspaceContext?: (query: string) => Promise<WorkspaceContextMatch[]>;
  onReadWorkspaceContext?: (path: string) => Promise<WorkspaceContextAttachment>;
}) {
  const { t } = useTranslation("conversation");
  const [value, setValue] = useState("");
  const [attachments, setAttachments] = useState<PromptAttachment[]>([]);
  const [attachmentError, setAttachmentError] = useState<Error | string>();
  const [linkOpen, setLinkOpen] = useState(false);
  const [linkUrl, setLinkUrl] = useState("");
  const [linkName, setLinkName] = useState("");
  const [dragActive, setDragActive] = useState(false);
  const [pendingFiles, setPendingFiles] = useState(0);
  const [commandMenuDismissed, setCommandMenuDismissed] = useState(false);
  const [activeCommandIndex, setActiveCommandIndex] = useState(0);
  const [caret, setCaret] = useState(0);
  const [contextMenuDismissed, setContextMenuDismissed] = useState(false);
  const [contextMatches, setContextMatches] = useState<WorkspaceContextMatch[]>([]);
  const [contextLoading, setContextLoading] = useState(false);
  const [activeContextIndex, setActiveContextIndex] = useState(0);
  const [expanded, setExpanded] = useState(false);
  const [historyIndex, setHistoryIndex] = useState<number>();
  const area = useRef<HTMLTextAreaElement>(null);
  const picker = useRef<HTMLInputElement>(null);
  const attachmentsRef = useRef<PromptAttachment[]>([]);
  const pendingAttachmentBytes = useRef(0);
  const pendingFileCount = useRef(0);
  const contextSearchSequence = useRef(0);
  const historyIndexRef = useRef<number | undefined>(undefined);
  const historyScratch = useRef<{
    value: string;
    attachments: PromptAttachment[];
  } | undefined>(undefined);
  const restoredBlocksRef = useRef<ContentBlock[] | undefined>(undefined);
  attachmentsRef.current = attachments;
  const attachmentsSupported = Boolean(
    capabilities?.image || capabilities?.audio || capabilities?.embeddedContext,
  );
  const commandMatches = useMemo(() => {
    if (commandMenuDismissed || !value.startsWith("/") || value.includes(" ")) return [];
    const query = value.slice(1).toLowerCase();
    return commands.filter(({ name }) => name.toLowerCase().includes(query)).slice(0, 8);
  }, [commandMenuDismissed, commands, value]);
  const contextMention = useMemo(() => {
    if (
      contextMenuDismissed ||
      capabilities?.embeddedContext !== true ||
      !onSearchWorkspaceContext ||
      !onReadWorkspaceContext
    ) return undefined;
    return workspaceContextMention(value, caret);
  }, [
    capabilities?.embeddedContext,
    caret,
    contextMenuDismissed,
    onReadWorkspaceContext,
    onSearchWorkspaceContext,
    value,
  ]);
  const contextMenuOpen = contextMention != null;

  const leaveHistory = () => {
    if (historyIndexRef.current != null) setHistoryIndex(undefined);
    historyIndexRef.current = undefined;
    historyScratch.current = undefined;
  };

  const markRestoredBlocksChanged = () => {
    restoredBlocksRef.current = undefined;
    leaveHistory();
  };

  const restoreComposer = (
    restoredValue: string,
    restoredAttachments: PromptAttachment[],
    restoredBlocks?: ContentBlock[],
  ) => {
    setValue(restoredValue);
    attachmentsRef.current = restoredAttachments;
    setAttachments(restoredAttachments);
    restoredBlocksRef.current = restoredBlocks;
    setAttachmentError(undefined);
    setDragActive(false);
    setLinkOpen(false);
    setCommandMenuDismissed(false);
    setContextMenuDismissed(false);
    setCaret(restoredValue.length);
    requestAnimationFrame(() => {
      area.current?.focus();
      area.current?.setSelectionRange(restoredValue.length, restoredValue.length);
      if (area.current) {
        area.current.style.height = "auto";
        area.current.style.height = `${Math.min(area.current.scrollHeight, 220)}px`;
      }
    });
  };

  const restoreBlocks = (blocks: ContentBlock[]) => {
    const restoredText = blocks
      .filter((block): block is Extract<ContentBlock, { type: "text" }> => block.type === "text")
      .map(({ text }) => text)
      .join("\n\n");
    const restoredAttachments = blocks
      .filter((block) => block.type !== "text")
      .map((block, index) => promptAttachment(block, index));
    restoreComposer(restoredText, restoredAttachments, blocks);
  };

  const navigateHistory = (direction: -1 | 1): boolean => {
    const current = historyIndexRef.current;
    if (direction < 0) {
      if (history.length === 0) return false;
      if (current == null) {
        if (value.length > 0 || attachmentsRef.current.length > 0) return false;
        historyScratch.current = {
          value,
          attachments: [...attachmentsRef.current],
        };
      }
      const next = current == null
        ? history.length - 1
        : Math.max(0, current - 1);
      historyIndexRef.current = next;
      setHistoryIndex(next);
      restoreBlocks(history[next]);
      return true;
    }

    if (current == null) return false;
    if (current < history.length - 1) {
      const next = current + 1;
      historyIndexRef.current = next;
      setHistoryIndex(next);
      restoreBlocks(history[next]);
      return true;
    }

    const scratch = historyScratch.current ?? { value: "", attachments: [] };
    historyIndexRef.current = undefined;
    historyScratch.current = undefined;
    setHistoryIndex(undefined);
    restoreComposer(scratch.value, scratch.attachments);
    return true;
  };

  useEffect(() => setActiveCommandIndex(0), [commandMatches.length, value]);
  useEffect(() => setActiveContextIndex(0), [contextMention?.query, contextMatches.length]);
  useEffect(() => {
    if (interactionPending) setExpanded(false);
  }, [interactionPending]);

  useEffect(() => {
    const sequence = ++contextSearchSequence.current;
    if (!contextMention || !onSearchWorkspaceContext) {
      setContextMatches([]);
      setContextLoading(false);
      return;
    }
    setContextLoading(true);
    const timer = window.setTimeout(() => {
      void onSearchWorkspaceContext(contextMention.query)
        .then((matches) => {
          if (contextSearchSequence.current !== sequence) return;
          setContextMatches(matches);
          setContextLoading(false);
        })
        .catch((error) => {
          if (contextSearchSequence.current !== sequence) return;
          setContextMatches([]);
          setContextLoading(false);
          setAttachmentError(error instanceof Error ? error : String(error));
        });
    }, 120);
    return () => window.clearTimeout(timer);
  }, [contextMention?.query, onSearchWorkspaceContext]);

  useEffect(() => {
    if (!draft) return;
    leaveHistory();
    restoreBlocks(draft.blocks);
  }, [draft]);

  const selectCommand = (command: AvailableCommand) => {
    markRestoredBlocksChanged();
    setValue(`/${command.name}${command.input ? " " : ""}`);
    setCommandMenuDismissed(true);
    area.current?.focus();
  };

  const toggleExpanded = () => {
    if (interactionPending) return;
    setExpanded((current) => !current);
    requestAnimationFrame(() => area.current?.focus());
  };

  const submit = () => {
    const text = value.trim();
    const currentAttachments = attachmentsRef.current;
    if ((!text && currentAttachments.length === 0) || disabled) return;
    if (pendingFileCount.current > 0) {
      setAttachmentError(new PromptAttachmentError("preparing"));
      return;
    }
    const currentBlocks = currentAttachments.map(({ block }) => block);
    const restoredBlocks = restoredBlocksRef.current;
    const accepted = restoredBlocks
      ? onSubmit(text, currentBlocks, restoredBlocks)
      : onSubmit(text, currentBlocks);
    if (accepted === false) return;
    setValue("");
    setAttachments([]);
    attachmentsRef.current = [];
    restoredBlocksRef.current = undefined;
    leaveHistory();
    setAttachmentError(undefined);
    setLinkOpen(false);
    setLinkUrl("");
    setLinkName("");
    setCommandMenuDismissed(false);
    if (area.current) area.current.style.height = "auto";
  };

  const attachFiles = async (files: File[]) => {
    if (files.length === 0) return;
    if (disabled) {
      setAttachmentError(new PromptAttachmentError("inactiveSession"));
      return;
    }
    if (!attachmentsSupported) {
      setAttachmentError(new PromptAttachmentError("unsupportedInput"));
      return;
    }
    const incomingBytes = files.reduce((sum, file) => sum + file.size, 0);
    let reserved = false;
    try {
      const attachedBytes = attachmentsRef.current.reduce(
        (sum, item) => sum + item.size,
        0,
      );
      if (
        attachedBytes + pendingAttachmentBytes.current + incomingBytes >
        MAX_ATTACHMENT_BYTES
      ) {
        throw new PromptAttachmentError("sizeLimit");
      }
      pendingAttachmentBytes.current += incomingBytes;
      pendingFileCount.current += files.length;
      reserved = true;
      setPendingFiles(pendingFileCount.current);
      const next = await createPromptAttachments(files, capabilities);
      const combined = [...attachmentsRef.current, ...next];
      markRestoredBlocksChanged();
      attachmentsRef.current = combined;
      setAttachments(combined);
      setAttachmentError(undefined);
    } catch (error) {
      setAttachmentError(error instanceof Error ? error : String(error));
    } finally {
      if (reserved) {
        pendingAttachmentBytes.current = Math.max(
          0,
          pendingAttachmentBytes.current - incomingBytes,
        );
        pendingFileCount.current = Math.max(0, pendingFileCount.current - files.length);
        setPendingFiles(pendingFileCount.current);
      }
    }
  };

  const attachWorkspaceContext = async (match: WorkspaceContextMatch) => {
    if (!onReadWorkspaceContext) return;
    const incomingBytes = match.size;
    let reservedBytes = 0;
    let reserved = false;
    try {
      const attachedBytes = attachmentsRef.current.reduce(
        (sum, item) => sum + item.size,
        0,
      );
      if (
        attachedBytes + pendingAttachmentBytes.current + incomingBytes >
        MAX_ATTACHMENT_BYTES
      ) {
        throw new PromptAttachmentError("sizeLimit");
      }
      pendingAttachmentBytes.current += incomingBytes;
      pendingFileCount.current += 1;
      reservedBytes = incomingBytes;
      reserved = true;
      setPendingFiles(pendingFileCount.current);
      const attachment = await onReadWorkspaceContext(match.path);
      const otherPendingBytes = Math.max(
        0,
        pendingAttachmentBytes.current - reservedBytes,
      );
      const currentBytes = attachmentsRef.current.reduce(
        (sum, item) => sum + item.size,
        0,
      );
      if (currentBytes + otherPendingBytes + attachment.size > MAX_ATTACHMENT_BYTES) {
        throw new PromptAttachmentError("sizeLimit");
      }
      pendingAttachmentBytes.current += attachment.size - reservedBytes;
      reservedBytes = attachment.size;
      const next: PromptAttachment = {
        id: randomId(),
        name: attachment.name,
        size: attachment.size,
        block: attachment.block,
      };
      const combined = [...attachmentsRef.current, next];
      markRestoredBlocksChanged();
      attachmentsRef.current = combined;
      setAttachments(combined);
      setAttachmentError(undefined);
    } catch (error) {
      setAttachmentError(error instanceof Error ? error : String(error));
    } finally {
      if (reserved) {
        pendingAttachmentBytes.current = Math.max(
          0,
          pendingAttachmentBytes.current - reservedBytes,
        );
        pendingFileCount.current = Math.max(0, pendingFileCount.current - 1);
        setPendingFiles(pendingFileCount.current);
      }
    }
  };

  const selectWorkspaceContext = (match: WorkspaceContextMatch) => {
    const mention = contextMention;
    if (!mention) return;
    const nextValue = `${value.slice(0, mention.start)}${value.slice(mention.end)}`;
    const nextCaret = mention.start;
    markRestoredBlocksChanged();
    setValue(nextValue);
    setCaret(nextCaret);
    setContextMenuDismissed(true);
    setContextMatches([]);
    void attachWorkspaceContext(match);
    requestAnimationFrame(() => {
      area.current?.focus();
      area.current?.setSelectionRange(nextCaret, nextCaret);
    });
  };

  const selectFiles = (event: ChangeEvent<HTMLInputElement>) => {
    const files = [...(event.target.files ?? [])];
    event.target.value = "";
    void attachFiles(files);
  };

  const pasteFiles = (event: ClipboardEvent<HTMLTextAreaElement>) => {
    const files = [...event.clipboardData.files];
    if (files.length === 0) return;
    event.preventDefault();
    void attachFiles(files);
  };

  const dragContainsFiles = (event: DragEvent<HTMLDivElement>): boolean =>
    Array.from(event.dataTransfer.types).includes("Files");

  const dragEnter = (event: DragEvent<HTMLDivElement>) => {
    if (!dragContainsFiles(event)) return;
    event.preventDefault();
    if (
      event.relatedTarget instanceof Node &&
      event.currentTarget.contains(event.relatedTarget)
    ) return;
    setDragActive(true);
  };

  const dragOver = (event: DragEvent<HTMLDivElement>) => {
    if (!dragContainsFiles(event)) return;
    event.preventDefault();
    event.dataTransfer.dropEffect = disabled || !attachmentsSupported ? "none" : "copy";
  };

  const dragLeave = (event: DragEvent<HTMLDivElement>) => {
    if (
      event.relatedTarget instanceof Node &&
      event.currentTarget.contains(event.relatedTarget)
    ) return;
    setDragActive(false);
  };

  const dropFiles = (event: DragEvent<HTMLDivElement>) => {
    if (!dragContainsFiles(event)) return;
    event.preventDefault();
    setDragActive(false);
    void attachFiles([...event.dataTransfer.files]);
  };

  const addResourceLink = () => {
    const uri = linkUrl.trim();
    if (!uri) return;
    try {
      new URL(uri);
    } catch {
      setAttachmentError(new PromptAttachmentError("absoluteUri"));
      return;
    }
    const fallbackName = uri.split("/").filter(Boolean).at(-1) ?? uri;
    const name = linkName.trim() || fallbackName;
    const next = [
      ...attachmentsRef.current,
      {
        id: randomId(),
        name,
        size: 0,
        block: {
          type: "resource_link" as const,
          uri,
          name,
        },
      },
    ];
    markRestoredBlocksChanged();
    attachmentsRef.current = next;
    setAttachments(next);
    setLinkUrl("");
    setLinkName("");
    setLinkOpen(false);
    setAttachmentError(undefined);
  };

  const onKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (
      event.key === "Escape" &&
      event.altKey &&
      event.shiftKey &&
      !event.ctrlKey &&
      !event.metaKey
    ) {
      event.preventDefault();
      toggleExpanded();
      return;
    }
    const navigation = keyboardNavigationTarget(event);
    if (navigation && onNavigateThread) {
      event.preventDefault();
      onNavigateThread(navigation);
      return;
    }
    if (contextMenuOpen) {
      if (event.key === "ArrowDown" || event.key === "ArrowUp") {
        event.preventDefault();
        if (contextMatches.length > 0) {
          const direction = event.key === "ArrowDown" ? 1 : -1;
          setActiveContextIndex((index) =>
            (index + direction + contextMatches.length) % contextMatches.length
          );
        }
        return;
      }
      if ((event.key === "Enter" || event.key === "Tab") && contextMatches.length > 0) {
        event.preventDefault();
        selectWorkspaceContext(
          contextMatches[activeContextIndex] ?? contextMatches[0],
        );
        return;
      }
      if (event.key === "Escape") {
        event.preventDefault();
        setContextMenuDismissed(true);
        return;
      }
    }
    if (commandMatches.length > 0) {
      if (event.key === "ArrowDown" || event.key === "ArrowUp") {
        event.preventDefault();
        const direction = event.key === "ArrowDown" ? 1 : -1;
        setActiveCommandIndex((index) =>
          (index + direction + commandMatches.length) % commandMatches.length
        );
        return;
      }
      if (event.key === "Enter") {
        event.preventDefault();
        selectCommand(commandMatches[activeCommandIndex] ?? commandMatches[0]);
        return;
      }
      if (event.key === "Escape") {
        event.preventDefault();
        setCommandMenuDismissed(true);
        return;
      }
    }
    if (event.key === "Escape" && running) {
      event.preventDefault();
      onCancel();
      return;
    }
    if (event.key === "ArrowUp" || event.key === "ArrowDown") {
      const handled = navigateHistory(event.key === "ArrowUp" ? -1 : 1);
      if (handled) {
        event.preventDefault();
        return;
      }
    }
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      submit();
    }
  };

  return (
    <div
      className={`composer${dragActive ? " drag-active" : ""}${expanded ? " composer-expanded" : ""}`}
      data-expanded={expanded}
      onDragEnter={dragEnter}
      onDragOver={dragOver}
      onDragLeave={dragLeave}
      onDrop={dropFiles}
    >
      {connectionRecovery ? (
        <div className="composer-reconnect" role="status" aria-live="polite">
          <WifiOff size={17} aria-hidden="true" />
          <span>
            <strong>{t("composer.connectionUnavailable")}</strong>
            <small>{connectionRecovery.message}</small>
          </span>
          <button type="button" onClick={connectionRecovery.onReconnect}>
            <RefreshCw size={13} aria-hidden="true" /> {t("composer.reconnect")}
          </button>
        </div>
      ) : null}
      {dragActive ? (
        <div className="attachment-drop-target" role="status">
          <Paperclip size={18} />
          <strong>{attachmentsSupported ? t("composer.drop.add") : t("composer.drop.unavailable")}</strong>
          <span>{attachmentsSupported
            ? t("composer.drop.supported")
            : t("composer.drop.unsupported")}</span>
        </div>
      ) : null}
      {contextMenuOpen ? (
        <div
          className="context-menu"
          id="workspace-context-menu"
          role="listbox"
          aria-label={t("composer.context.label")}
        >
          <header><AtSign size={13} /><span>{t("composer.context.files")}</span></header>
          {contextLoading ? (
            <div className="context-menu-state" role="status">
              <LoaderCircle className="spin" size={12} /> {t("composer.context.searching")}
            </div>
          ) : contextMatches.length > 0 ? contextMatches.map((match, index) => (
            <button
              key={match.path}
              id={`workspace-context-${index}`}
              type="button"
              role="option"
              aria-selected={index === activeContextIndex}
              onMouseDown={(event) => event.preventDefault()}
              onMouseEnter={() => setActiveContextIndex(index)}
              onClick={() => selectWorkspaceContext(match)}
            >
              <FileText size={13} />
              <span><strong>{match.name}</strong><small>{match.relativePath}</small></span>
              <code>{formatBytes(match.size)}</code>
            </button>
          )) : (
            <div className="context-menu-state">{t("composer.context.noMatches")}</div>
          )}
          <footer><kbd>↑↓</kbd> {t("composer.context.navigate")} <kbd>Enter</kbd> {t("composer.context.add")} <kbd>Esc</kbd> {t("composer.context.close")}</footer>
        </div>
      ) : null}
      {commandMatches.length > 0 ? (
        <div className="command-menu" id="agent-command-menu" role="listbox" aria-label={t("composer.commands")}>
          {commandMatches.map((command, index) => (
            <button
              key={command.name}
              id={`agent-command-${index}`}
              type="button"
              role="option"
              aria-selected={index === activeCommandIndex}
              onMouseEnter={() => setActiveCommandIndex(index)}
              onClick={() => selectCommand(command)}
            >
              <code>/{command.name}</code>
              <span>{command.description}</span>
              {command.input ? <small>{command.input.hint}</small> : null}
            </button>
          ))}
        </div>
      ) : null}
      {attachments.length > 0 ? (
        <div className="attachment-list">
          {attachments.map((attachment) => (
            <span key={attachment.id}>
              <AttachmentIcon block={attachment.block} />
              <span>{attachment.name}</span>
              <small>{attachmentLabel(attachment)}</small>
              <button
                type="button"
                aria-label={t("attachments.remove", { name: attachment.name })}
                onClick={() => {
                  const next = attachmentsRef.current.filter(({ id }) => id !== attachment.id);
                  markRestoredBlocksChanged();
                  attachmentsRef.current = next;
                  setAttachments(next);
                }}
              >
                <X size={11} />
              </button>
            </span>
          ))}
        </div>
      ) : null}
      {attachmentError ? <div className="attachment-error">{promptAttachmentErrorMessage(attachmentError)}</div> : null}
      {pendingFiles > 0 ? (
        <div className="attachment-loading" role="status">
          <LoaderCircle className="spin" size={12} />
          {t("attachments.preparing", { count: pendingFiles })}
        </div>
      ) : null}
      {linkOpen ? (
        <div className="link-editor">
          <input aria-label={t("composer.link.uri")} placeholder={t("composer.link.uriPlaceholder")} value={linkUrl} onChange={(event) => setLinkUrl(event.target.value)} />
          <input aria-label={t("composer.link.name")} placeholder={t("composer.link.namePlaceholder")} value={linkName} onChange={(event) => setLinkName(event.target.value)} />
          <button type="button" aria-label={t("composer.link.add")} disabled={!linkUrl.trim()} onClick={addResourceLink}><Plus size={13} /></button>
        </div>
      ) : null}
      <textarea
        ref={area}
        rows={1}
        value={value}
        disabled={disabled}
        role="combobox"
        aria-keyshortcuts={running ? "Alt+Shift+Escape Escape" : "Alt+Shift+Escape"}
        aria-autocomplete="list"
        aria-controls={contextMenuOpen ? "workspace-context-menu" : "agent-command-menu"}
        aria-expanded={contextMenuOpen || commandMatches.length > 0}
        aria-activedescendant={contextMenuOpen && contextMatches.length > 0
          ? `workspace-context-${activeContextIndex}`
          : commandMatches.length > 0
            ? `agent-command-${activeCommandIndex}`
            : undefined}
        placeholder={disabled
          ? t("composer.placeholder.disabled")
          : running
            ? t("composer.placeholder.running")
            : t("composer.placeholder.ready")}
        onChange={(event) => {
          markRestoredBlocksChanged();
          setValue(event.target.value);
          setCaret(event.target.selectionStart);
          setCommandMenuDismissed(false);
          setContextMenuDismissed(false);
          event.target.style.height = "auto";
          event.target.style.height = `${Math.min(event.target.scrollHeight, 220)}px`;
        }}
        onSelect={(event) => setCaret(event.currentTarget.selectionStart)}
        onPaste={pasteFiles}
        onKeyDown={onKeyDown}
      />
      <div className="composer-bar">
        <input
          ref={picker}
          type="file"
          hidden
          multiple
          accept={capabilities?.embeddedContext ? undefined : [capabilities?.image ? "image/*" : "", capabilities?.audio ? "audio/*" : ""].filter(Boolean).join(",")}
          onChange={selectFiles}
        />
        <button
          type="button"
          className="icon-button"
          aria-label={t("composer.attachments.attach")}
          disabled={disabled || !attachmentsSupported}
          title={attachmentsSupported ? t("composer.attachments.attach") : t("composer.attachments.unavailable")}
          onClick={() => picker.current?.click()}
        >
          <Paperclip size={16} />
        </button>
        <button
          type="button"
          className="icon-button"
          aria-label={t("composer.link.addAcp")}
          disabled={disabled}
          title={t("composer.link.addAcp")}
          onClick={() => setLinkOpen((open) => !open)}
        >
          <Link2 size={15} />
        </button>
        {sessionControls}
        <ContextUsage usage={usage} />
        <button
          type="button"
          className="icon-button composer-expand-button"
          aria-label={expanded ? t("composer.expand.collapseLabel") : t("composer.expand.expandLabel")}
          aria-keyshortcuts="Alt+Shift+Escape"
          aria-pressed={expanded}
          disabled={interactionPending}
          title={interactionPending
            ? t("composer.expand.pending")
            : t(expanded ? "composer.expand.collapseHint" : "composer.expand.expandHint")}
          onClick={toggleExpanded}
        >
          {expanded ? <Minimize2 size={15} /> : <Maximize2 size={15} />}
        </button>
        <span>{historyIndex != null
          ? t("composer.history", { index: historyIndex + 1, count: history.length })
          : running
          ? t("composer.hint.queue")
          : capabilities?.embeddedContext
            ? t("composer.hint.filesAndCommands")
            : commands.length > 0
              ? t("composer.hint.commands")
            : t("composer.hint.send")}</span>
        {running ? (
          <button
            type="button"
            className="send-button stop-button"
            aria-label={t("composer.stop")}
            onClick={onCancel}
            title={t("composer.cancel")}
          >
            <Square size={13} fill="currentColor" />
          </button>
        ) : (
          <button
            className="send-button"
            aria-label={t("composer.send")}
            disabled={disabled || pendingFiles > 0 || (!value.trim() && attachments.length === 0)}
            onClick={submit}
          >
            <CornerDownLeft size={16} />
          </button>
        )}
      </div>
    </div>
  );
}

function keyboardNavigationTarget(
  event: KeyboardEvent<HTMLTextAreaElement>,
): ThreadNavigationTarget | undefined {
  if (!event.ctrlKey) return undefined;
  if (event.altKey) {
    if (event.key === "ArrowUp") return "previous-message";
    if (event.key === "ArrowDown") return "next-message";
    if (event.key === "PageUp") return "previous-prompt";
    if (event.key === "PageDown") return "next-prompt";
    return undefined;
  }
  if (event.key === "Home") return "top";
  if (event.key === "End") return "bottom";
  if (event.key === "PageUp") return "page-up";
  if (event.key === "PageDown") return "page-down";
  return undefined;
}

function workspaceContextMention(
  value: string,
  caret: number,
): { start: number; end: number; query: string } | undefined {
  if (caret < 0 || caret > value.length) return undefined;
  const prefix = value.slice(0, caret);
  const match = prefix.match(/(?:^|\s)@([^\s@]{0,256})$/);
  if (!match) return undefined;
  const query = match[1];
  const start = caret - query.length - 1;
  let end = caret;
  while (end < value.length && !/\s/.test(value[end])) end += 1;
  return { start, end, query };
}

function formatBytes(value: number): string {
  const digits = value < 10240 ? 1 : 0;
  return value < 1024
    ? i18n.t("attachments.bytes", { ns: "conversation", value: value.toLocaleString(i18n.language) })
    : i18n.t("attachments.kilobytes", {
        ns: "conversation",
        value: (value / 1024).toLocaleString(i18n.language, { minimumFractionDigits: digits, maximumFractionDigits: digits }),
      });
}

function AttachmentIcon({ block }: { block: ContentBlock }) {
  switch (block.type) {
    case "image": return <ImageIcon size={12} />;
    case "audio": return <Music2 size={12} />;
    case "resource": return <FileText size={12} />;
    case "resource_link": return <Link2 size={12} />;
    case "text": return <File size={12} />;
  }
}

function attachmentLabel(attachment: PromptAttachment): string {
  const kind = attachment.block.type === "resource_link"
    ? "link"
    : attachment.block.type === "resource"
      ? "context"
      : attachment.block.type;
  const label = i18n.t(`attachments.kind.${kind}`, { ns: "conversation" });
  return attachment.size
    ? i18n.t("attachments.description", { ns: "conversation", kind: label, size: formatBytes(attachment.size) })
    : label;
}

function promptAttachment(block: Exclude<ContentBlock, { type: "text" }>, index: number): PromptAttachment {
  switch (block.type) {
    case "image":
      return { id: `draft:${index}`, name: block.uri ?? block.mimeType, size: decodedSize(block.data), block };
    case "audio":
      return { id: `draft:${index}`, name: block.mimeType, size: decodedSize(block.data), block };
    case "resource_link":
      return { id: `draft:${index}`, name: block.title ?? block.name, size: block.size ?? 0, block };
    case "resource": {
      const size = "text" in block.resource
        ? new TextEncoder().encode(block.resource.text).byteLength
        : decodedSize(block.resource.blob);
      return { id: `draft:${index}`, name: block.resource.uri, size, block };
    }
  }
}

function decodedSize(base64: string): number {
  const padding = base64.endsWith("==") ? 2 : base64.endsWith("=") ? 1 : 0;
  return Math.max(0, Math.floor(base64.length / 4) * 3 - padding);
}
