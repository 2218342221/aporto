import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';

export function Markdown({ children }: { children: string }) {
  return (
    <div className="markdown">
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        skipHtml
        components={{
          a: ({ href, children: label }) => (
            <a href={href} target="_blank" rel="noreferrer noopener">
              {label}
            </a>
          ),
          // Workspace output is untrusted: render an image label/link without auto-fetching remote content.
          img: ({ src, alt }) => (
            <a
              href={typeof src === 'string' ? src : undefined}
              target="_blank"
              rel="noreferrer noopener"
            >
              [图片：{alt || '打开图片'}]
            </a>
          ),
        }}
      >
        {children}
      </ReactMarkdown>
    </div>
  );
}
