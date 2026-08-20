import type { ReactNode } from "react";

/**
 * Deliberately tiny markdown renderer: bold, italic, inline code, links,
 * bullet/numbered lists, paragraphs. Everything is rendered as React text
 * nodes — no HTML is ever parsed or injected, so untrusted model/indexer
 * text cannot become markup.
 */
export function Markdown({ text }: { text: string }) {
  const blocks = text.trim().split(/\n{2,}/);
  return (
    <div className="space-y-2.5">
      {blocks.map((block, i) => {
        const lines = block.split("\n");
        const isBullet = lines.every((l) => /^\s*[-*•]\s+/.test(l));
        const isNumbered = lines.every((l) => /^\s*\d+[.)]\s+/.test(l));

        if (isBullet || isNumbered) {
          const items = lines.map((l) => l.replace(/^\s*(?:[-*•]|\d+[.)])\s+/, ""));
          const Tag = isNumbered ? "ol" : "ul";
          return (
            <Tag
              key={i}
              className={
                isNumbered
                  ? "ml-4 list-decimal space-y-1 marker:text-faint"
                  : "ml-4 list-disc space-y-1 marker:text-accent/60"
              }
            >
              {items.map((it, j) => (
                <li key={j} className="pl-0.5">
                  <Inline text={it} />
                </li>
              ))}
            </Tag>
          );
        }

        return (
          <p key={i} className="leading-relaxed">
            {lines.map((l, j) => (
              <span key={j}>
                {j > 0 && <br />}
                <Inline text={l} />
              </span>
            ))}
          </p>
        );
      })}
    </div>
  );
}

/** Splits on `code`, **bold**, *italic*, and bare URLs — in that precedence. */
function Inline({ text }: { text: string }) {
  const out: ReactNode[] = [];
  const re = /(`[^`]+`)|(\*\*[^*]+\*\*)|(\*[^*]+\*)|(https?:\/\/[^\s<>()]+)/g;
  let last = 0;
  let m: RegExpExecArray | null;
  let key = 0;

  while ((m = re.exec(text)) !== null) {
    if (m.index > last) out.push(text.slice(last, m.index));
    const tok = m[0];
    if (tok.startsWith("`")) {
      out.push(
        <code
          key={key++}
          className="rounded-[5px] border border-line2/70 bg-bg0/70 px-1.5 py-px font-mono text-[11.5px] text-accent"
        >
          {tok.slice(1, -1)}
        </code>,
      );
    } else if (tok.startsWith("**")) {
      out.push(
        <strong key={key++} className="font-semibold text-ink">
          {tok.slice(2, -2)}
        </strong>,
      );
    } else if (tok.startsWith("*")) {
      out.push(
        <em key={key++} className="italic">
          {tok.slice(1, -1)}
        </em>,
      );
    } else {
      out.push(
        <a
          key={key++}
          href={tok}
          target="_blank"
          rel="noreferrer noopener"
          className="text-accent underline decoration-accent/30 underline-offset-2 hover:decoration-accent"
        >
          {tok.length > 48 ? `${tok.slice(0, 45)}…` : tok}
        </a>,
      );
    }
    last = m.index + tok.length;
  }
  if (last < text.length) out.push(text.slice(last));
  return <>{out}</>;
}
