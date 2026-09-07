/**
 * A status pill: a colored dot plus its text label, always both — never
 * color alone, so state reads the same for a colorblind viewer as anyone
 * else (AGENTS.md-adjacent principle for this whole app: never hide state
 * behind color only).
 */
const TONES = {
  good: "bg-good/15 text-good",
  warn: "bg-warn/15 text-warn",
  bad: "bg-bad/15 text-bad",
  neutral: "bg-ink-faint/15 text-ink-muted",
} as const;

export function Badge({
  tone,
  children,
}: {
  tone: keyof typeof TONES;
  children: React.ReactNode;
}) {
  return (
    <span
      className={`inline-flex items-center gap-1.5 rounded px-2 py-0.5 text-xs font-medium ${TONES[tone]}`}
    >
      <span
        className={`h-1.5 w-1.5 rounded-full ${
          tone === "good"
            ? "bg-good"
            : tone === "warn"
              ? "bg-warn"
              : tone === "bad"
                ? "bg-bad"
                : "bg-ink-faint"
        }`}
      />
      {children}
    </span>
  );
}
