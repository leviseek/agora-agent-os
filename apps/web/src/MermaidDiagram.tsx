/**
 * A Mermaid diagram from a fenced code block, rendered as SVG.
 *
 * Two things make this more than a one-liner:
 *
 *  * the library is imported on demand. Mermaid is the largest dependency in this console, and a
 *    session that never contains a diagram should not pay for it: the import happens the first
 *    time a ```mermaid block appears, and the block shows a short placeholder until it lands.
 *  * the diagram source is untrusted. It comes from a model, which may have copied it from a page
 *    it read, so the renderer runs with `securityLevel: 'strict'`: no HTML labels, no click
 *    handlers, no links that reach out of the diagram.
 *
 * A diagram that does not parse is shown as the source it is - an error box, not a blank space.
 */
import { useEffect, useRef, useState } from 'react';

export interface MermaidDiagramProps {
  /** The diagram source, exactly as it appeared inside the fence. */
  source: string;
  /** 'dark' or 'light'; the diagram is themed to match the console. */
  theme: 'dark' | 'light';
  /** Extra class for the wrapper. */
  className?: string;
}

interface MermaidApi {
  initialize: (config: Record<string, unknown>) => void;
  render: (id: string, source: string) => Promise<{ svg: string }>;
}

let loader: Promise<MermaidApi> | null = null;

/** Load mermaid once, configured the way this console needs it. */
function loadMermaid(theme: 'dark' | 'light'): Promise<MermaidApi> {
  if (loader === null) {
    loader = import('mermaid').then((module) => {
      const api = (module.default ?? module) as unknown as MermaidApi;
      return api;
    });
  }
  return loader.then((api) => {
    api.initialize({
      startOnLoad: false,
      // Untrusted input: no HTML labels, no click handlers, no external links.
      securityLevel: 'strict',
      theme: theme === 'dark' ? 'dark' : 'default',
      fontFamily: 'inherit',
      flowchart: { htmlLabels: false },
    });
    return api;
  });
}

let sequence = 0;

export function MermaidDiagram({ source, theme, className }: MermaidDiagramProps) {
  const [svg, setSvg] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const hostRef = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    let cancelled = false;
    setError(null);
    loadMermaid(theme)
      .then((api) => api.render('mermaid-' + ++sequence, source))
      .then((result) => {
        if (!cancelled) setSvg(result.svg);
      })
      .catch((cause: unknown) => {
        if (!cancelled) setError(cause instanceof Error ? cause.message : String(cause));
      });
    return () => {
      cancelled = true;
    };
  }, [source, theme]);

  if (error !== null) {
    return (
      <div className={className === undefined ? 'mermaid-error' : 'mermaid-error ' + className}>
        <p className="muted small">this diagram did not parse ({error})</p>
        <pre>{source}</pre>
      </div>
    );
  }

  if (svg === null) {
    return (
      <div className={className === undefined ? 'mermaid-pending' : 'mermaid-pending ' + className}>
        <p className="muted small">rendering diagram...</p>
      </div>
    );
  }

  return (
    <div
      className={className === undefined ? 'mermaid' : 'mermaid ' + className}
      ref={hostRef}
      // The SVG came from mermaid, which sanitises under securityLevel 'strict'.
      dangerouslySetInnerHTML={{ __html: svg }}
    />
  );
}
