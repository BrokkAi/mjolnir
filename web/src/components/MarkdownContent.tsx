import { Check, Copy } from 'lucide-react';
import { memo, useCallback, useState, type ReactNode } from 'react';
import ReactMarkdown, { type Components } from 'react-markdown';
import rehypeSanitize, { defaultSchema } from 'rehype-sanitize';
import remarkGfm from 'remark-gfm';

export interface MarkdownContentProps {
  text: string;
  className?: string;
  compact?: boolean;
}

const SANITIZE_SCHEMA = {
  ...defaultSchema,
  protocols: {
    ...defaultSchema.protocols,
    href: ['http', 'https', 'mailto'],
  },
};

function CodeBlock({ language, code }: { language?: string; code: string }) {
  const [copied, setCopied] = useState(false);

  const onCopy = useCallback(() => {
    void navigator.clipboard?.writeText(code).then(() => {
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1500);
    });
  }, [code]);

  return (
    <div className="my-2 overflow-hidden rounded-lg border border-line bg-raised [overflow-anchor:none]">
      <div className="flex items-center justify-between border-b border-line/70 px-3 py-1.5 text-xs text-muted">
        <span className="font-mono">{language || 'code'}</span>
        <button
          type="button"
          onClick={onCopy}
          aria-label="Copy code"
          className="inline-flex min-h-7 items-center gap-1 rounded px-1.5 text-xs text-muted hover:bg-surface hover:text-fg"
        >
          {copied ? <Check className="size-3.5 text-ok" aria-hidden /> : <Copy className="size-3.5" aria-hidden />}
          <span>{copied ? 'Copied' : 'Copy'}</span>
        </button>
      </div>
      <pre className="overflow-x-auto overflow-y-hidden p-3 font-mono text-[13px] leading-relaxed text-fg">
        <code>{code}</code>
      </pre>
    </div>
  );
}

function extractText(children: ReactNode): string {
  if (typeof children === 'string') return children;
  if (Array.isArray(children)) return children.map(extractText).join('');
  return '';
}

const COMPONENTS: Components = {
  pre({ children }) {
    const child = Array.isArray(children) ? children[0] : children;
    if (
      child &&
      typeof child === 'object' &&
      'props' in child &&
      child.props &&
      typeof child.props === 'object'
    ) {
      const props = child.props as { className?: string; children?: ReactNode };
      const match = /language-([^\s]+)/.exec(props.className ?? '');
      const code = extractText(props.children).replace(/\n$/, '');
      return <CodeBlock language={match?.[1]} code={code} />;
    }
    return (
      <pre className="my-2 overflow-x-auto overflow-y-hidden rounded-lg border border-line bg-raised p-3 font-mono text-[13px] leading-relaxed [overflow-anchor:none]">
        {children}
      </pre>
    );
  },
  code({ className, children, ...rest }) {
    const isBlock = Boolean(className?.includes('language-'));
    if (isBlock) {
      return (
        <code className={className} {...rest}>
          {children}
        </code>
      );
    }
    return (
      <code className="rounded bg-raised px-1.5 py-0.5 font-mono text-[0.9em] text-fg" {...rest}>
        {children}
      </code>
    );
  },
  a({ href, children, ...rest }) {
    return (
      <a
        href={href}
        target="_blank"
        rel="noopener noreferrer"
        className="text-accent underline decoration-accent/50 underline-offset-2 hover:decoration-accent"
        {...rest}
      >
        {children}
      </a>
    );
  },
  table({ children }) {
    return (
      <div className="my-2 overflow-x-auto overflow-y-hidden rounded-lg border border-line [overflow-anchor:none]">
        <table className="w-full border-collapse text-left text-xs">{children}</table>
      </div>
    );
  },
  th({ children }) {
    return (
      <th className="border-b border-line bg-raised px-2.5 py-1.5 font-semibold text-fg">
        {children}
      </th>
    );
  },
  td({ children }) {
    return <td className="border-b border-line/60 px-2.5 py-1.5 align-top">{children}</td>;
  },
  blockquote({ children }) {
    return (
      <blockquote className="my-2 border-l-2 border-accent/60 pl-3 text-muted">
        {children}
      </blockquote>
    );
  },
  ul({ children }) {
    return <ul className="my-1.5 list-disc space-y-1 pl-5">{children}</ul>;
  },
  ol({ children }) {
    return <ol className="my-1.5 list-decimal space-y-1 pl-5">{children}</ol>;
  },
  h1({ children }) {
    return <h1 className="mt-3 mb-1.5 text-base font-semibold text-fg">{children}</h1>;
  },
  h2({ children }) {
    return <h2 className="mt-3 mb-1 text-[15px] font-semibold text-fg">{children}</h2>;
  },
  h3({ children }) {
    return <h3 className="mt-2.5 mb-1 text-sm font-semibold text-fg">{children}</h3>;
  },
  p({ children }) {
    return <p className="my-1.5 first:mt-0 last:mb-0">{children}</p>;
  },
  hr() {
    return <hr className="my-3 border-line" />;
  },
};

export const MarkdownContent = memo(function MarkdownContent({
  text,
  className = '',
  compact = false,
}: MarkdownContentProps) {
  return (
    <div
      className={`min-w-0 break-words text-fg ${compact ? 'text-[13px] leading-snug' : 'text-[14.5px] leading-relaxed'} ${className}`}
    >
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        rehypePlugins={[[rehypeSanitize, SANITIZE_SCHEMA]]}
        components={COMPONENTS}
      >
        {text}
      </ReactMarkdown>
    </div>
  );
});
