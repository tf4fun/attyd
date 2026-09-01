import {
  CaseSensitive,
  ChevronLeft,
  ChevronRight,
  Regex,
  WholeWord,
  X,
} from "lucide-react";
import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type RefObject,
  type ReactNode,
} from "react";
import {
  MAX_THREAD_SEARCH_CHARS,
  MAX_THREAD_SEARCH_MATCHES,
  MAX_THREAD_SEARCH_QUERY_CHARS,
  compileThreadSearch,
  type ThreadSearchOptions,
} from "../../lib/thread-search";

const SEARCH_HIGHLIGHT = "attyd-thread-search-match";
const ACTIVE_SEARCH_HIGHLIGHT = "attyd-thread-search-active";
const STREAM_UPDATE_DEBOUNCE_MS = 150;

interface TextSegment {
  node: Text;
  start: number;
  end: number;
}

interface SearchDocument {
  key: string;
  entry: HTMLElement;
  text: string;
  segments: TextSegment[];
}

interface DomSearchMatch {
  key: string;
  entry: HTMLElement;
  range?: Range;
}

interface DomSearchResult {
  matches: DomSearchMatch[];
  error?: string;
  limited: boolean;
}

export function ThreadSearchBar({
  rootRef,
  containerRef,
  contentVersion,
  terminalVersion,
  focusRequest,
  onClose,
}: {
  rootRef: RefObject<HTMLElement | null>;
  containerRef: RefObject<HTMLDivElement | null>;
  contentVersion: unknown;
  terminalVersion: unknown;
  focusRequest: number;
  onClose: () => void;
}) {
  const [query, setQuery] = useState("");
  const [options, setOptions] = useState<ThreadSearchOptions>({
    caseSensitive: false,
    wholeWord: false,
    regex: false,
  });
  const [matches, setMatches] = useState<DomSearchMatch[]>([]);
  const [activeIndex, setActiveIndex] = useState<number>();
  const [error, setError] = useState<string>();
  const [limited, setLimited] = useState(false);
  const [disclosureVersion, setDisclosureVersion] = useState(0);
  const input = useRef<HTMLInputElement>(null);
  const matchesRef = useRef<DomSearchMatch[]>([]);
  const activeIndexRef = useRef<number | undefined>(undefined);
  const highlightedEntries = useRef<Set<HTMLElement>>(new Set());
  const previousConfig = useRef("");

  const applyActiveMatch = useCallback((nextMatches: DomSearchMatch[], nextIndex?: number) => {
    clearHighlights(highlightedEntries.current);
    const nextEntries = new Set(nextMatches.map(({ entry }) => entry));
    for (const entry of nextEntries) entry.dataset.threadSearchHit = "true";
    const active = nextIndex == null ? undefined : nextMatches[nextIndex];
    if (active) active.entry.dataset.threadSearchActive = "true";
    highlightedEntries.current = nextEntries;
    paintRanges(nextMatches, nextIndex);
  }, []);

  const scan = useCallback(() => {
    const root = rootRef.current;
    const previousMatch = activeIndexRef.current == null
      ? undefined
      : matchesRef.current[activeIndexRef.current];
    const result = root
      ? scanThreadSearchDom(root, query, options)
      : { matches: [], limited: false };
    let nextIndex: number | undefined;
    if (result.matches.length > 0) {
      const preserved = previousMatch == null
        ? -1
        : result.matches.findIndex(({ key }) => key === previousMatch.key);
      nextIndex = preserved >= 0
        ? preserved
        : Math.min(activeIndexRef.current ?? 0, result.matches.length - 1);
    }
    matchesRef.current = result.matches;
    activeIndexRef.current = nextIndex;
    setMatches(result.matches);
    setActiveIndex(nextIndex);
    setError(result.error);
    setLimited(result.limited);
    applyActiveMatch(result.matches, nextIndex);
    if (nextIndex != null && previousMatch == null) {
      scrollToEntry(result.matches[nextIndex]?.entry);
    }
  }, [applyActiveMatch, options, query, rootRef]);

  useEffect(() => {
    const config = `${query}\u0000${Number(options.caseSensitive)}${Number(options.wholeWord)}${Number(options.regex)}`;
    const immediate = previousConfig.current !== config;
    previousConfig.current = config;
    if (immediate) {
      scan();
      return;
    }
    const timer = window.setTimeout(scan, STREAM_UPDATE_DEBOUNCE_MS);
    return () => window.clearTimeout(timer);
  }, [contentVersion, disclosureVersion, options, query, scan, terminalVersion]);

  useEffect(() => {
    input.current?.focus();
    input.current?.select();
  }, [focusRequest]);

  useEffect(() => {
    const root = rootRef.current;
    if (!root) return;
    const onToggle = () => setDisclosureVersion((version) => version + 1);
    root.addEventListener("toggle", onToggle, true);
    return () => root.removeEventListener("toggle", onToggle, true);
  }, [rootRef]);

  useEffect(() => () => {
    clearHighlights(highlightedEntries.current);
    clearPaintedRanges();
  }, []);

  const activate = (direction: -1 | 1) => {
    const currentMatches = matchesRef.current;
    if (currentMatches.length === 0) return;
    const current = activeIndexRef.current;
    const next = current == null
      ? direction > 0 ? 0 : currentMatches.length - 1
      : (current + direction + currentMatches.length) % currentMatches.length;
    activeIndexRef.current = next;
    setActiveIndex(next);
    applyActiveMatch(currentMatches, next);
    scrollToEntry(currentMatches[next]?.entry);
    input.current?.focus();
  };

  const toggleOption = (option: keyof ThreadSearchOptions) => {
    setOptions((current) => ({ ...current, [option]: !current[option] }));
  };

  const hasMatches = matches.length > 0;
  const invalid = error != null || (query.length > 0 && !hasMatches);
  const counter = query.length === 0
    ? ""
    : `${activeIndex == null ? 0 : activeIndex + 1}/${matches.length}`;

  return (
    <div
      ref={containerRef}
      className="thread-search-bar"
      role="search"
      aria-label="Search this Agent thread"
      onKeyDown={(event) => {
        if (event.key === "Escape") {
          event.preventDefault();
          onClose();
        } else if (event.key === "Enter") {
          event.preventDefault();
          activate(event.shiftKey ? -1 : 1);
        } else if (event.key === "F3") {
          event.preventDefault();
          activate(event.shiftKey ? -1 : 1);
        }
      }}
    >
      <div className={`thread-search-input${invalid ? " invalid" : ""}`}>
        <input
          ref={input}
          autoFocus
          type="search"
          aria-label="Search this thread"
          aria-invalid={error != null}
          aria-describedby={error || limited ? "thread-search-status" : undefined}
          placeholder="Search this thread…"
          value={query}
          maxLength={MAX_THREAD_SEARCH_QUERY_CHARS + 1}
          onChange={(event) => setQuery(event.target.value)}
        />
        <SearchOptionButton
          active={options.caseSensitive}
          label="Match case"
          onClick={() => toggleOption("caseSensitive")}
        ><CaseSensitive size={14} /></SearchOptionButton>
        <SearchOptionButton
          active={options.wholeWord}
          label="Match whole word"
          onClick={() => toggleOption("wholeWord")}
        ><WholeWord size={14} /></SearchOptionButton>
        <SearchOptionButton
          active={options.regex}
          label="Use regular expression"
          onClick={() => toggleOption("regex")}
        ><Regex size={13} /></SearchOptionButton>
      </div>
      <div className="thread-search-navigation">
        <button
          type="button"
          aria-label="Previous thread search match"
          title="Previous match · Shift+Enter"
          disabled={!hasMatches}
          onClick={() => activate(-1)}
        ><ChevronLeft size={15} /></button>
        <button
          type="button"
          aria-label="Next thread search match"
          title="Next match · Enter"
          disabled={!hasMatches}
          onClick={() => activate(1)}
        ><ChevronRight size={15} /></button>
        <output aria-live="polite">{counter}</output>
        <button
          type="button"
          aria-label="Close thread search"
          title="Close search · Escape"
          onClick={onClose}
        ><X size={15} /></button>
      </div>
      {error || limited ? (
        <div id="thread-search-status" className={error ? "thread-search-error" : "thread-search-limit"} role="status">
          {error ?? `Showing the first ${MAX_THREAD_SEARCH_MATCHES.toLocaleString()} matches`}
        </div>
      ) : null}
    </div>
  );
}

function SearchOptionButton({
  active,
  label,
  onClick,
  children,
}: {
  active: boolean;
  label: string;
  onClick: () => void;
  children: ReactNode;
}) {
  return (
    <button
      type="button"
      aria-label={label}
      aria-pressed={active}
      title={label}
      onClick={onClick}
    >{children}</button>
  );
}

export function scanThreadSearchDom(
  root: HTMLElement,
  query: string,
  options: ThreadSearchOptions,
): DomSearchResult {
  const compiled = compileThreadSearch(query, options);
  if (compiled.error) return { matches: [], error: compiled.error, limited: false };
  if (query.length === 0) return { matches: [], limited: false };

  const documents = collectSearchDocuments(root);
  const matches: DomSearchMatch[] = [];
  let searchedCharacters = 0;
  let limited = false;
  for (const document of documents) {
    if (matches.length >= MAX_THREAD_SEARCH_MATCHES) {
      limited = true;
      break;
    }
    const remainingCharacters = MAX_THREAD_SEARCH_CHARS - searchedCharacters;
    if (remainingCharacters <= 0) {
      limited = true;
      break;
    }
    const text = document.text.slice(0, remainingCharacters);
    searchedCharacters += text.length;
    if (text.length < document.text.length) limited = true;
    const result = compiled.find(text, MAX_THREAD_SEARCH_MATCHES - matches.length);
    limited ||= result.limited;
    for (const match of result.matches) {
      matches.push({
        key: `${document.key}:${match.start}:${match.end}`,
        entry: document.entry,
        range: createDomRange(document, match.start, match.end),
      });
    }
  }
  return { matches, limited };
}

function collectSearchDocuments(root: HTMLElement): SearchDocument[] {
  const elements = [...root.querySelectorAll<HTMLElement>("[data-thread-searchable]")];
  const documents: SearchDocument[] = [];
  let documentIndex = 0;
  for (const element of elements) {
    if (!searchElementIsVisible(element)) continue;
    if (element.parentElement?.closest("[data-thread-searchable]")) continue;
    const entry = element.closest<HTMLElement>("[data-thread-entry-id]");
    if (!entry) continue;
    const segments: TextSegment[] = [];
    let text = "";
    const walker = element.ownerDocument.createTreeWalker(element, NodeFilter.SHOW_TEXT, {
      acceptNode(node) {
        const parent = node.parentElement;
        if (
          !parent ||
          parent.closest("[data-thread-search-ignore]") ||
          parent.closest("script, style")
        ) return NodeFilter.FILTER_REJECT;
        return node.textContent?.length ? NodeFilter.FILTER_ACCEPT : NodeFilter.FILTER_REJECT;
      },
    });
    let node: Node | null;
    while ((node = walker.nextNode()) != null) {
      const value = node.textContent ?? "";
      const start = text.length;
      text += value;
      segments.push({ node: node as Text, start, end: text.length });
    }
    if (text.length === 0) continue;
    documents.push({
      key: `${entry.dataset.threadEntryId ?? "entry"}:${documentIndex}`,
      entry,
      text,
      segments,
    });
    documentIndex += 1;
  }
  return documents;
}

function searchElementIsVisible(element: HTMLElement): boolean {
  if (element.closest("[hidden]")) return false;
  const closed = element.closest<HTMLDetailsElement>("details:not([open])");
  if (!closed) return true;
  const summary = closed.querySelector(":scope > summary");
  return summary?.contains(element) === true;
}

function createDomRange(document: SearchDocument, start: number, end: number): Range | undefined {
  const startSegment = document.segments.find((segment) =>
    start >= segment.start && start < segment.end
  );
  const endOffset = end - 1;
  const endSegment = document.segments.find((segment) =>
    endOffset >= segment.start && endOffset < segment.end
  );
  if (!startSegment || !endSegment) return undefined;
  try {
    const range = startSegment.node.ownerDocument.createRange();
    range.setStart(startSegment.node, start - startSegment.start);
    range.setEnd(endSegment.node, end - endSegment.start);
    return range;
  } catch {
    return undefined;
  }
}

function paintRanges(matches: DomSearchMatch[], activeIndex?: number) {
  clearPaintedRanges();
  const registry = highlightRegistry();
  const Highlight = highlightConstructor();
  if (!registry || !Highlight) return;
  const inactive = matches
    .filter((_, index) => index !== activeIndex)
    .map(({ range }) => range)
    .filter((range): range is Range => range != null);
  const active = activeIndex == null ? undefined : matches[activeIndex]?.range;
  if (inactive.length > 0) registry.set(SEARCH_HIGHLIGHT, new Highlight(...inactive));
  if (active) registry.set(ACTIVE_SEARCH_HIGHLIGHT, new Highlight(active));
}

function clearPaintedRanges() {
  const registry = highlightRegistry();
  registry?.delete(SEARCH_HIGHLIGHT);
  registry?.delete(ACTIVE_SEARCH_HIGHLIGHT);
}

function clearHighlights(entries: Set<HTMLElement>) {
  for (const entry of entries) {
    delete entry.dataset.threadSearchHit;
    delete entry.dataset.threadSearchActive;
  }
}

function scrollToEntry(entry: HTMLElement | undefined) {
  if (!entry?.isConnected || typeof entry.scrollIntoView !== "function") return;
  entry.scrollIntoView({ block: "center", behavior: "smooth" });
}

interface HighlightRegistry {
  set(name: string, highlight: unknown): void;
  delete(name: string): void;
}

function highlightRegistry(): HighlightRegistry | undefined {
  if (typeof CSS === "undefined") return undefined;
  return (CSS as typeof CSS & { highlights?: HighlightRegistry }).highlights;
}

function highlightConstructor(): (new (...ranges: Range[]) => unknown) | undefined {
  if (typeof window === "undefined") return undefined;
  return (window as typeof window & {
    Highlight?: new (...ranges: Range[]) => unknown;
  }).Highlight;
}
