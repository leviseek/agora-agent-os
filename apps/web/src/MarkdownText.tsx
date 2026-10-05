/**
 * Markdown, rendered for the console.
 *
 * Models answer in Markdown whether or not anybody asks them to: headings, tables, lists and fenced
 * code run through almost every real answer, and printing that source verbatim means the reader
 * sees the scaffolding instead of the answer. This renders it.
 *
 * Three decisions worth stating:
 *
 *  * raw HTML is NOT rendered. react-markdown ignores it unless a plugin re-enables it, and no such
 *    plugin is used: an answer is untrusted text from a model, and a model that echoes a page it
 *    read must not be able to inject markup into the console.
 *  * a link may only be http, https or mailto. Anything else - javascript:, data: - is shown as the
 *    text it is rather than made clickable.
 *  * links open in a new tab with rel="noreferrer noopener", so an answer cannot reach back into
 *    the app through window.opener.
 */
import Markdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import { memo } from 'react';
import { MermaidDiagram } from './MermaidDiagram';
import { useTheme } from './theme';

/** A link target that is safe to make clickable. */
const SAFE_TARGET = /^(https?:|mailto:|\/)/i;

export interface MarkdownTextProps {
  /** The Markdown source. */
  text: string;
  /** Extra class for the wrapper, so a caller can size it (a bubble, a preview, a cell). */
  className?: string;
}

/**
 * Memoised on purpose: a streaming answer redraws on every chunk, and re-parsing an unchanged
 * prefix hundreds of times is work nobody asked for.
 */
export const MarkdownText = memo(function MarkdownText({ text, className }: MarkdownTextProps) {
  // A diagram has to be drawn in the palette that is on screen, and the palette is React state.
  const { theme } = useTheme();
  return (
    <div className={className === undefined ? 'md' : 'md ' + className}>
      <Markdown
        remarkPlugins={[remarkGfm]}
        // react-markdown sanitises URLs with its own transform; the component overrides below are
        // the guard that a reader can see, and this keeps them the only thing that decides.
        urlTransform={(url) => url}
        components={{
          a({ href, children, ...rest }) {
            const safe = typeof href === 'string' && SAFE_TARGET.test(href.trim());
            if (!safe) {
              return <span className="md-inert-link">{children}</span>;
            }
            return (
              <a href={href} target="_blank" rel="noreferrer noopener" {...rest}>
                {children}
              </a>
            );
          },
          img({ src, alt, ...rest }) {
            const safe = typeof src === 'string' && SAFE_TARGET.test(src.trim());
            if (!safe) return <span className="md-inert-link">{alt ?? 'image'}</span>;
            return <img src={src} alt={alt ?? ''} loading="lazy" {...rest} />;
          },
          pre({ children, ...rest }) {
            // A fenced block is a <pre> around a <code class="language-x">. The language is on the
            // child, so a mermaid fence is recognised here and replaced by the picture - leaving the
            // <pre> in place would draw a monospace box around a diagram.
            const child = (Array.isArray(children) ? children[0] : children) as
              | { props?: { className?: string; children?: unknown } }
              | undefined;
            const language = /language-([\w-]+)/.exec(child?.props?.className ?? '')?.[1];
            if (language === 'mermaid') {
              const source = String(child?.props?.children ?? '').trim();
              return <MermaidDiagram source={source} theme={theme} />;
            }
            return <pre {...rest}>{children}</pre>;
          },
          table({ children, ...rest }) {
            // A wide table scrolls on its own instead of stretching the transcript.
            return (
              <div className="md-table-wrap">
                <table {...rest}>{children}</table>
              </div>
            );
          },
        }}
      >
        {text}
      </Markdown>
    </div>
  );
});
