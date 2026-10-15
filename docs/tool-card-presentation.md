# Tool card presentation

These are client presentation rules for [ACP v1 tool calls](https://agentclientprotocol.com/protocol/v1/tool-calls), not additional protocol requirements. Every tool uses the same card structure, regardless of Agent or tool name.

## Summary and disclosure

- Display the Agent's `title` without splitting it into invented fields. Use the tool name, category, or “Tool” only when the title is empty.
- Use `kind` for the icon and category label. Missing or unrecognized categories use the generic icon; they do not select an output schema.
- Display the reported tool status. Pending can mean streaming input or awaiting permission; do not assume which.
- If a turn is cancelled while a tool is pending or running, show a local cancelled state instead
  of an endless spinner. Preserve the Agent's wire status and allow later Agent updates to resolve it.
- Start cards collapsed and preserve the user's disclosure choice as updates arrive. Expanded titles remain readable at narrow widths.

## Inputs and results

- Show supplied inputs in a consistent structured view without interpreting tool-specific parameter names.
- Render all `content` blocks in their reported order: readable text and media, resources, file differences, or terminal output.
- Treat whitespace-only text blocks as absent output. Preserve empty file diffs, scalar results, and annotations in Tool info.
- When no content is supplied, use `rawOutput` as the result. When both are supplied, keep `rawOutput` in a collapsed **Additional output** section. They may contain different information; neither overrides nor replaces the other.
- Preserve valid scalar results such as `0`, `false`, and empty strings. Do not infer a vendor-specific schema from tool names or JSON keys.
- Distinguish pending, running, completed without output, and failed without error details. Empty content is not evidence of success or failure.

## Files and terminals

- Show a file path where it identifies a diff or resource. Keep separate `locations` in **Tool info**, avoiding another repeated file list in the result.
- Compute actual changed lines from diff snapshots using the same bounded algorithm as Changes. Preserve unchanged context, label approximate counts and omitted rows, and recognize `oldText: null` as a new file. Empty `newText` does not establish file deletion.
- Label embedded output **Terminal**; retain terminal IDs in Tool info. Tool status and process exit status remain independent.
- Treat a present terminal `exitStatus` as finished, even without a code or signal. Show waiting text only while a process can still produce output; completed empty output and unavailable snapshots need distinct messages.
- Indicate truncation. See [ACP terminals](https://agentclientprotocol.com/protocol/v1/terminals).
- Terminal output is display data in the session's `live.terminals` while the Agent keeps the terminal. Release removes it with its output, and a completed turn keeps no tool calls, so output is shown only while its turn runs or its terminal is unreleased ([protocol §4.6](protocol.md)). Unavailable output is not reconstructed or written into Agent message fields.

## Media and attachments

- Native image/audio and embedded text/binary resources use the same compact attachment entry.
  Tool results use plain file rows; message attachments use compact chips. File names, available
  sizes, and tool-result locations remain readable, with MIME/address/description in the tooltip.
- Open available attachments with one ordinary link to a Blob URL the page builds from the read
  block and revokes when the view goes. Safe media (common images, audio, video, and text or JSON
  as plain text) open in a new page; HTML, SVG, XML, PDF and unknown types are downloads with the
  original bytes. Do not add inline media players or PDF/Office renderers.
- Documents contain references to content already held in the bridge's session memory.
  The attachment read resolves a reference while the session is held and its log or `live`
  refers to it, without a separate store or filesystem lookup; the page caches a read block
  only while a view shows it. Session
  release or bridge restart ends that availability unless the Agent supplies the content again
  during history restoration.
- Resource links can open HTTP(S) URLs or attyd-hosted attachment references. Other URI schemes
  remain descriptive entries. Do not fetch remote bytes just to build a preview or download action.
  Invalid payloads have no enabled open action.
- Keep protocol diagnostics in folded details. Inline payloads are omitted from attachment views.
  Markdown export is assembled from the log and its attachments, as `canon` normalizes them:
  `_meta` is dropped, and tool calls are not part of it.

## Technical details

Keep IDs, file locations, content annotations, and raw message events in **Tool info**. These remain available for diagnosis without appearing as ordinary task results. Hiding an annotation never removes its associated content.

Permission cards reuse tool-content rendering, including diffs and terminal snapshots, and apply
the same null/omission semantics to inherited input. Cancellation intent comes from the running
operation's `cancelRequested` before the final prompt response. See the
[2026-09-26 display audit](acp-display-audit-2026-09-26.md) for the original findings and follow-up.
