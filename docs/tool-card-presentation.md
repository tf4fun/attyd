# Tool card presentation

These are client presentation rules for [ACP v1 tool calls](https://agentclientprotocol.com/protocol/v1/tool-calls), not additional protocol requirements. Every tool uses the same card structure, regardless of Agent or tool name.

## Summary and disclosure

- Display the Agent's `title` without splitting it into invented fields. Use the tool name, category, or “Tool” only when the title is empty.
- Use `kind` for the icon and category label. Missing or unrecognized categories use the generic icon; they do not select an output schema.
- Display the reported tool status. Pending can mean streaming input or awaiting permission; do not assume which.
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
- Preserve output after release and indicate truncation. See [ACP terminals](https://agentclientprotocol.com/protocol/v1/terminals).
- Terminal output snapshots are display data held only in bridge memory while the session remains materialized. Closing the session or ending the bridge process clears them. Cold history restoration depends on the Agent; unavailable output is not reconstructed or written into Agent message fields.

## Technical details

Keep IDs, file locations, content annotations, and raw message events in **Tool info**. These remain available for diagnosis without appearing as ordinary task results. Hiding an annotation never removes its associated content.
