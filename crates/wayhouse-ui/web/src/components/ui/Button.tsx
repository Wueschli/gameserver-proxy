import { forwardRef } from "react";

type Variant = "primary" | "secondary" | "danger" | "ghost";

const VARIANTS: Record<Variant, string> = {
  primary: "bg-accent text-accent-ink hover:brightness-110",
  secondary: "bg-surface-raised text-ink border border-line hover:border-ink-faint",
  danger: "bg-bad/15 text-bad border border-bad/30 hover:bg-bad/25",
  ghost: "text-ink-muted hover:text-ink hover:bg-surface-raised",
};

export const Button = forwardRef<
  HTMLButtonElement,
  React.ButtonHTMLAttributes<HTMLButtonElement> & { variant?: Variant }
>(function Button({ variant = "secondary", className = "", ...props }, ref) {
  return (
    <button
      ref={ref}
      className={`inline-flex items-center justify-center gap-1.5 rounded px-3 py-1.5 text-sm font-medium
        transition-colors disabled:cursor-not-allowed disabled:opacity-50 ${VARIANTS[variant]} ${className}`}
      {...props}
    />
  );
});

export function Input(props: React.InputHTMLAttributes<HTMLInputElement>) {
  return (
    <input
      {...props}
      className={`rounded border border-line bg-surface px-2.5 py-1.5 text-sm text-ink placeholder:text-ink-faint
        focus:border-accent ${props.className ?? ""}`}
    />
  );
}
