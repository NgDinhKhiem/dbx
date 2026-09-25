import { Marked, type Tokens } from "marked";

const EXTERNAL_LINK_PROTOCOLS = new Set(["http:", "https:"]);

export interface AiMarkdownLinkClickEvent {
  target: unknown;
  currentTarget: unknown;
  preventDefault: () => void;
  stopPropagation: () => void;
}

export type AiMarkdownLinkOpener = (url: string) => void | Promise<void>;

const markedInstance = new Marked({
  breaks: true,
  gfm: true,
  renderer: {
    codespan({ text }: Tokens.Codespan) {
      return `<code class="rounded bg-muted px-1.5 py-0.5 text-[11px] font-mono">${escapeHtml(text)}</code>`;
    },
    html({ text }: Tokens.HTML | Tokens.Tag) {
      return escapeHtml(text);
    },
    link({ href, title, tokens }: Tokens.Link) {
      const label = this.parser.parseInline(tokens);
      const safeHref = normalizeAiMarkdownLink(href);
      if (!safeHref) return label;

      const titleAttr = title ? ` title="${escapeHtml(title)}"` : "";
      return `<a href="${escapeHtml(safeHref)}"${titleAttr} target="_blank" rel="noopener noreferrer">${label}</a>`;
    },
    image({ href, title, text }: Tokens.Image) {
      return renderAiMarkdownImage(href, title, text);
    },
    table({ header, rows, align }: Tokens.Table) {
      const renderCell = (cell: Tokens.TableCell, tag: "th" | "td", colIndex: number): string => {
        const content = this.parser.parseInline(cell.tokens);
        const alignAttr = align?.[colIndex] ? ` align="${escapeHtml(align[colIndex]!)}"` : "";
        return `<${tag}${alignAttr}>${content}</${tag}>`;
      };
      const thead = header.length > 0 ? `<thead><tr>${header.map((cell, i) => renderCell(cell, "th", i)).join("")}</tr></thead>` : "";
      const tbodyRows = rows
        .map((row) => {
          const cells = row.map((cell, i) => renderCell(cell, "td", i)).join("");
          return `<tr>${cells}</tr>`;
        })
        .join("");
      const tbody = `<tbody>${tbodyRows}</tbody>`;
      return `<div class="ai-markdown-table-wrap"><table>${thead}${tbody}</table></div>`;
    },
  },
});

// Inline raster images only. Remote images are never auto-loaded: an `<img>`
// pointing at an attacker URL is a zero-click exfiltration channel when a
// prompt-injected answer smuggles data into the query string.
const SAFE_INLINE_IMAGE_SOURCE = /^data:image\/(?:png|jpe?g|gif|webp|avif);base64,[a-z0-9+/=\s]+$/i;

export function renderAiMarkdownImage(href: string, title: string | null | undefined, text: string): string {
  const titleAttr = title ? ` title="${escapeHtml(title)}"` : "";
  const source = (href ?? "").trim();
  if (SAFE_INLINE_IMAGE_SOURCE.test(source)) {
    return `<img src="${escapeHtml(source.replace(/\s+/g, ""))}" alt="${escapeHtml(text)}"${titleAttr} loading="lazy" referrerpolicy="no-referrer" />`;
  }

  const safeHref = normalizeAiMarkdownLink(source);
  const label = escapeHtml(text || safeHref || "image");
  if (!safeHref) return label;
  // A plain link the user has to click: nothing is fetched on render.
  return `<a href="${escapeHtml(safeHref)}"${titleAttr} target="_blank" rel="noopener noreferrer" data-ai-markdown-image="true">${label}</a>`;
}

export function formatAiInlineMarkdown(text: string): string {
  try {
    return markedInstance.parse(text) as string;
  } catch {
    return escapeHtml(text);
  }
}

export function formatAiInlineMarkdownFragment(text: string): string {
  try {
    return markedInstance.parseInline(text) as string;
  } catch {
    return escapeHtml(text);
  }
}

export function normalizeAiMarkdownLink(href: string): string | null {
  try {
    const url = new URL(href);
    return EXTERNAL_LINK_PROTOCOLS.has(url.protocol) ? url.toString() : null;
  } catch {
    return null;
  }
}

export function aiMarkdownLinkUrlFromClick(target: unknown, currentTarget: unknown): string | null {
  const anchor = closestAnchor(target);
  if (!anchor) return null;
  if (hasContains(currentTarget) && !currentTarget.contains(anchor)) return null;

  const href = anchor.getAttribute("href");
  return href ? normalizeAiMarkdownLink(href) : null;
}

export function handleAiMarkdownLinkClick(event: AiMarkdownLinkClickEvent, openUrl: AiMarkdownLinkOpener): boolean {
  const url = aiMarkdownLinkUrlFromClick(event.target, event.currentTarget);
  if (!url) return false;

  event.preventDefault();
  event.stopPropagation();
  void openUrl(url);
  return true;
}

export function escapeHtml(value: string): string {
  return value.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
}

interface AnchorLike {
  getAttribute: (name: string) => string | null;
}

function closestAnchor(target: unknown): AnchorLike | null {
  const closestSource = hasClosest(target) ? target : parentWithClosest(target);
  if (!closestSource) return null;

  const anchor = closestSource.closest("a[href]");
  return hasGetAttribute(anchor) ? anchor : null;
}

function parentWithClosest(target: unknown): { closest: (selector: string) => unknown } | null {
  if (!target || typeof target !== "object" || !("parentElement" in target)) return null;
  const parentElement = target.parentElement;
  return hasClosest(parentElement) ? parentElement : null;
}

function hasClosest(value: unknown): value is { closest: (selector: string) => unknown } {
  return !!value && typeof value === "object" && "closest" in value && typeof value.closest === "function";
}

function hasGetAttribute(value: unknown): value is AnchorLike {
  return !!value && typeof value === "object" && "getAttribute" in value && typeof value.getAttribute === "function";
}

function hasContains(value: unknown): value is { contains: (node: unknown) => boolean } {
  return !!value && typeof value === "object" && "contains" in value && typeof value.contains === "function";
}
