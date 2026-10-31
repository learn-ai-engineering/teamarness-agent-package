import DOMPurify from "dompurify";
import MarkdownIt from "markdown-it";
import Token from "markdown-it/lib/token.mjs";

const SAFE_SCHEMES = /^(?:https?:|mailto:|tel:|#|\/|\.\.?\/)/i;
const SAFE_URI = /^(?:(?:https?|mailto|tel):|[^a-z][^:]*$|#|\/|\.\.?\/)/i;
const ALLOWED_TAGS = ["a", "blockquote", "br", "code", "del", "em", "h1", "h2", "h3", "h4", "hr", "li", "ol", "p", "pre", "strong", "ul"];
const ALLOWED_ATTR = ["href", "target", "rel"];

function safeLink(url: string): boolean {
  try {
    return SAFE_SCHEMES.test(decodeURIComponent(url).trim()) && SAFE_URI.test(url.trim());
  } catch {
    return false;
  }
}

function escapeText(value: string): string {
  return value.replaceAll("&", "&amp;").replaceAll("<", "&lt;").replaceAll(">", "&gt;").replaceAll('"', "&quot;");
}

const markdown = new MarkdownIt({ html: false, breaks: true, linkify: false, typographer: false });
markdown.validateLink = safeLink;
markdown.renderer.rules.image = (tokens: Token[], index: number): string => {
  const token = tokens[index];
  const alt = token?.children?.map((child) => child.content).join("") || token?.content || token?.attrGet("alt") || "";
  return escapeText(alt);
};
const linkRule = markdown.renderer.rules.link_open;
markdown.renderer.rules.link_open = (tokens, index, options, env, self): string => {
  const token = tokens[index];
  if (!token) return "";
  const href = token.attrGet("href") ?? "#";
  if (!safeLink(href)) token.attrSet("href", "#");
  token.attrSet("target", "_blank");
  token.attrSet("rel", "noopener noreferrer");
  return linkRule ? linkRule(tokens, index, options, env, self) : self.renderToken(tokens, index, options);
};

/** Render the intentionally small Markdown subset without HTML or fetchable media. */
export function renderMarkdown(source: string): string {
  const normalized = source.replace(/\]\(([^)]*)\)/g, (whole, destination: string) => {
    const url = destination.trim().split(/\s+/, 1)[0] ?? "";
    return safeLink(url) ? whole : "]()";
  });
  const rendered = markdown.render(normalized);
  if (typeof window === "undefined") return rendered;
  return DOMPurify.sanitize(rendered, {
    ALLOWED_TAGS,
    ALLOWED_ATTR,
    FORBID_TAGS: ["img", "audio", "video", "iframe", "object", "embed", "style", "script"],
    FORBID_ATTR: ["style", "src", "srcset", "onerror", "onclick"],
    ALLOW_DATA_ATTR: false,
    ALLOWED_URI_REGEXP: SAFE_URI,
  });
}
