import { useCallback, useRef, useState } from "react";
import { Button } from "./Button";
import { Dialog } from "./Dialog";

export interface ConfirmOptions {
  title: string;
  description?: string;
  confirmLabel?: string;
  /** Style the confirm button as destructive (default true — this exists to guard them). */
  danger?: boolean;
}

/**
 * Promise-based confirmation: `if (!(await confirm({...}))) return;`.
 * Render the returned `dialog` once in the component that calls `confirm`.
 * Cancel, Escape and clicking outside all resolve `false`.
 */
export function useConfirm() {
  const [opts, setOpts] = useState<ConfirmOptions | null>(null);
  const resolver = useRef<((ok: boolean) => void) | null>(null);

  const settle = useCallback((ok: boolean) => {
    resolver.current?.(ok);
    resolver.current = null;
    setOpts(null);
  }, []);

  const confirm = useCallback((o: ConfirmOptions) => {
    // A second ask while one is open cancels the first rather than leaking it.
    resolver.current?.(false);
    return new Promise<boolean>((resolve) => {
      resolver.current = resolve;
      setOpts(o);
    });
  }, []);

  const dialog = (
    <Dialog
      open={opts !== null}
      onOpenChange={(open) => {
        if (!open) settle(false);
      }}
      title={opts?.title ?? ""}
      description={opts?.description}
    >
      <div className="flex justify-end gap-2">
        <Button variant="ghost" onClick={() => settle(false)}>
          Cancel
        </Button>
        <Button variant={opts?.danger === false ? "primary" : "danger"} onClick={() => settle(true)}>
          {opts?.confirmLabel ?? "Confirm"}
        </Button>
      </div>
    </Dialog>
  );

  return { confirm, dialog };
}
