import { useId } from 'react';
import type { CSSProperties } from 'react';

const paths = {
  plus: 'M12 5v14M5 12h14',
  arrow: 'M12 19V5m-6 6 6-6 6 6',
  chevron: 'm9 5 7 7-7 7',
  close: 'm6 6 12 12M6 18 18 6',
  menu: 'M4 6h16M4 12h16M4 18h16',
  panel: 'M15 3v18M3 3h18v18H3zM18 8h.01M18 12h.01M18 16h.01',
  search: 'M21 21l-5.2-5.2M18 10a8 8 0 1 1-16 0 8 8 0 0 1 16 0',
  chat: 'M21 11.5a8.4 8.4 0 0 1-.9 3.8 8.5 8.5 0 0 1-7.6 4.7 8.4 8.4 0 0 1-3.8-.9L3 21l1.9-5.7a8.4 8.4 0 0 1-.9-3.8 8.5 8.5 0 0 1 4.7-7.6 8.4 8.4 0 0 1 3.8-.9h.5a8.5 8.5 0 0 1 8 8v.5z',
  check: 'm5 12 4 4L19 6',
  clock: 'M12 8v4l3 2M22 12a10 10 0 1 1-20 0 10 10 0 0 1 20 0',
  stop: 'M6 6h12v12H6z',
  bolt: 'm13 2-9 12h7l-1 8 10-12h-7l1-8z',
  box: 'm12 3 9 5v8l-9 5-9-5V8l9-5zm0 10 9-5M12 13 3 8m9 5v8m-4-16 9 5',
  link: 'm10 13 4-4m-6 7-1 1a4.2 4.2 0 0 1-6-6l5-5a4.2 4.2 0 0 1 6 0m0 0 1-1a4.2 4.2 0 0 1 6 6l-5 5a4.2 4.2 0 0 1-6 0',
  logout: 'M9 21H3V3h6m7 14 5-5-5-5M21 12H9',
  alert: 'm12 3 10 18H2L12 3zm0 5v5m0 4h.01',
  code: 'm8 7-5 5 5 5m8-10 5 5-5 5m-3-13-2 16',
  file: 'M14 2H4v20h16V8l-6-6zm0 0v6h6M8 13h8M8 17h5',
  terminal:
    'M4 3h16a1 1 0 0 1 1 1v16a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1V4a1 1 0 0 1 1-1zm3 5 4 4-4 4m7 0h3',
  edit: 'm16 3 5 5M4 20l4-1L21 6a2.1 2.1 0 0 0-3-3L5 16l-1 4z',
  copy: 'M9 9h11v12H9zM5 15H3V3h11v2',
  spark: 'm12 3 2.5 6.5L21 12l-6.5 2.5L12 21l-2.5-6.5L3 12l6.5-2.5L12 3z',
} as const;

export function Icon({
  name,
  size = 18,
  className,
  style,
}: {
  name: keyof typeof paths;
  size?: number;
  className?: string;
  style?: CSSProperties;
}) {
  return (
    <svg
      aria-hidden="true"
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.8"
      strokeLinecap="round"
      strokeLinejoin="round"
      className={className}
      style={style}
    >
      <path d={paths[name]} />
    </svg>
  );
}

export function BrandMark({ large = false }: { large?: boolean }) {
  const gradientId = useId();
  return (
    <span className={`brand-mark${large ? ' large' : ''}`} aria-hidden="true">
      <svg
        viewBox="0 0 40 40"
        fill="none"
        strokeWidth="2.8"
        strokeLinecap="round"
        strokeLinejoin="round"
      >
        <defs>
          <linearGradient id={gradientId} x1="0" y1="0" x2="1" y2="1">
            <stop offset="0%" stopColor="#779cff" />
            <stop offset="55%" stopColor="#b18cde" />
            <stop offset="100%" stopColor="#df839e" />
          </linearGradient>
        </defs>
        <path d="M10 29 20 10 30 29M15 22h10" stroke={`url(#${gradientId})`} />
      </svg>
    </span>
  );
}
