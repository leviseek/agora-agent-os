/**
 * A Mermaid diagram from a fenced code block, rendered as SVG, with the ways out of the console
 * a diagram should have: save it as a PNG, copy the image, copy the source.
 *
 * Three things make this more than a one-liner:
 *
 *  * the library is imported on demand. Mermaid is the largest dependency in this console, and a
 *    session that never contains a diagram should not pay for it: the import happens the first
 *    time a ```mermaid block appears, and the block shows a short placeholder until it lands.
 *  * the diagram source is untrusted. It comes from a model, which may have copied it from a page
 *    it read, so the renderer runs with `securityLevel: 'strict'`: no HTML from the source, no
 *    click handlers, no links that reach out of the diagram.
 *  * it has to be copyable. A diagram a reader cannot get out of the page is a picture they have to
 *    screenshot, so the export buttons are part of the feature and not a nicety.
 *
 * A diagram that does not parse is shown as the source it is - an error box, not a blank space.
 */
import { useCallback, useEffect, useRef, useState } from 'react';
import { useI18n } from './i18n';

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
    loader = import('mermaid').then((module) => (module.default ?? module) as unknown as MermaidApi);
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

/**
 * The diagram as a PNG, at twice the drawn size.
 *
 * Exported because this is the part worth testing on its own: turning an inline SVG into an image
 * means serialising it, decoding it through an <img>, drawing it on a canvas and encoding that -
 * four places where a diagram can silently come out blank, zero-sized or tainted.
 *
 * The background is painted rather than left transparent: a dark-theme diagram on a transparent
 * canvas is unreadable wherever it is pasted.
 */
export async function svgToPngBlob(
  svg: SVGSVGElement,
  theme: 'dark' | 'light',
  scale = 2,
): Promise<Blob> {
  const clone = svg.cloneNode(true) as SVGSVGElement;
  // mermaid writes width="100%" and a max-width style, which rasterises to a zero-width image.
  const box = svg.viewBox?.baseVal;
  const width = box && box.width > 0 ? box.width : svg.getBoundingClientRect().width || 800;
  const height = box && box.height > 0 ? box.height : svg.getBoundingClientRect().height || 600;
  clone.setAttribute('width', String(width));
  clone.setAttribute('height', String(height));
  clone.removeAttribute('style');
  clone.setAttribute('xmlns', 'http://www.w3.org/2000/svg');
  clone.setAttribute('xmlns:xlink', 'http://www.w3.org/1999/xlink');

  const serialized = new XMLSerializer().serializeToString(clone);
  // A data URL rather than a blob URL: an SVG loaded from a blob URL taints the canvas in some
  // browsers, and a tainted canvas cannot be read back - toBlob would throw.
  const source = 'data:image/svg+xml;charset=utf-8,' + encodeURIComponent(serialized);

  const image = new Image();
  await new Promise<void>((resolve, reject) => {
    image.onload = () => resolve();
    image.onerror = () => reject(new Error('the diagram could not be decoded as an image'));
    image.src = source;
  });

  const canvas = document.createElement('canvas');
  canvas.width = Math.max(1, Math.round(width * scale));
  canvas.height = Math.max(1, Math.round(height * scale));
  const context = canvas.getContext('2d');
  if (context === null) throw new Error('this browser has no 2d canvas context');
  context.fillStyle = theme === 'dark' ? '#111418' : '#ffffff';
  context.fillRect(0, 0, canvas.width, canvas.height);
  context.drawImage(image, 0, 0, canvas.width, canvas.height);

  const blob = await new Promise<Blob | null>((resolve) => canvas.toBlob(resolve, 'image/png'));
  if (blob === null) throw new Error('the PNG could not be encoded');
  return blob;
}

/** Save a blob under a file name. */
function download(blob: Blob, name: string): void {
  const url = URL.createObjectURL(blob);
  const anchor = document.createElement('a');
  anchor.href = url;
  anchor.download = name;
  anchor.click();
  // Revoked on a later tick: revoking immediately can cancel the download in some browsers.
  window.setTimeout(() => URL.revokeObjectURL(url), 1_000);
}

export function MermaidDiagram({ source, theme, className }: MermaidDiagramProps) {
  const { t } = useI18n();
  const [svg, setSvg] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [note, setNote] = useState<string | null>(null);
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

  const withDiagram = useCallback(async (): Promise<SVGSVGElement | null> => {
    const found = hostRef.current?.querySelector('svg');
    return found instanceof SVGSVGElement ? found : null;
  }, []);

  const onSavePng = useCallback(async (): Promise<void> => {
    setNote(null);
    try {
      const diagram = await withDiagram();
      if (diagram === null) throw new Error('the diagram is not on the page');
      download(await svgToPngBlob(diagram, theme), 'diagram.png');
      setNote(t('chat.diagramSaved'));
    } catch (cause) {
      setNote(t('chat.diagramFailed', { reason: cause instanceof Error ? cause.message : String(cause) }));
    }
  }, [t, theme, withDiagram]);

  const onCopyPng = useCallback(async (): Promise<void> => {
    setNote(null);
    try {
      const diagram = await withDiagram();
      if (diagram === null) throw new Error('the diagram is not on the page');
      const blob = await svgToPngBlob(diagram, theme);
      if (typeof ClipboardItem === 'undefined' || navigator.clipboard?.write === undefined) {
        throw new Error('this browser cannot put an image on the clipboard');
      }
      await navigator.clipboard.write([new ClipboardItem({ 'image/png': blob })]);
      setNote(t('chat.diagramCopied'));
    } catch (cause) {
      setNote(t('chat.diagramFailed', { reason: cause instanceof Error ? cause.message : String(cause) }));
    }
  }, [t, theme, withDiagram]);

  const onCopySource = useCallback(async (): Promise<void> => {
    setNote(null);
    try {
      await navigator.clipboard.writeText(source);
      setNote(t('chat.diagramSourceCopied'));
    } catch (cause) {
      setNote(t('chat.diagramFailed', { reason: cause instanceof Error ? cause.message : String(cause) }));
    }
  }, [source, t]);

  if (error !== null) {
    return (
      <div className={className === undefined ? 'mermaid-error' : 'mermaid-error ' + className}>
        <p className="muted small">{t('chat.diagramInvalid', { reason: error })}</p>
        <pre>{source}</pre>
      </div>
    );
  }

  if (svg === null) {
    return (
      <div className={className === undefined ? 'mermaid-pending' : 'mermaid-pending ' + className}>
        <p className="muted small">{t('chat.diagramRendering')}</p>
      </div>
    );
  }

  return (
    <figure className={className === undefined ? 'mermaid-figure' : 'mermaid-figure ' + className}>
      <div
        className="mermaid"
        ref={hostRef}
        // The SVG came from mermaid, which sanitises the source under securityLevel 'strict'.
        dangerouslySetInnerHTML={{ __html: svg }}
      />
      <div className="mermaid-actions">
        <button type="button" className="btn btn-ghost btn-small" onClick={() => void onSavePng()}>
          {t('chat.diagramSavePng')}
        </button>
        <button type="button" className="btn btn-ghost btn-small" onClick={() => void onCopyPng()}>
          {t('chat.diagramCopyPng')}
        </button>
        <button type="button" className="btn btn-ghost btn-small" onClick={() => void onCopySource()}>
          {t('chat.diagramCopySource')}
        </button>
        {note !== null ? <span className="muted small">{note}</span> : null}
      </div>
    </figure>
  );
}
