import { useEffect, useState } from "react";
import { Button } from "./ui/Button";
import { Dialog } from "./ui/Dialog";

export const EXTERNAL_RISK_TEXT =
  "This registry is external, at your own risk. A sniffer from it runs inside every proxy that accepts it " +
  "(sandboxed, with the limits shown), and nobody on this project has reviewed it.";

/**
 * Asks the operator to accept the external-registry risk before an action
 * that trusts it (adding the registry, installing from it). Confirm stays
 * disabled until the box is ticked; reopening starts unticked.
 */
export function RegistryRiskDialog({
  open,
  onOpenChange,
  title,
  confirmLabel,
  onConfirm,
  busy = false,
  children,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  title: string;
  confirmLabel: string;
  onConfirm: () => void;
  busy?: boolean;
  children?: React.ReactNode;
}) {
  const [accepted, setAccepted] = useState(false);
  useEffect(() => {
    if (!open) setAccepted(false);
  }, [open]);

  return (
    <Dialog open={open} onOpenChange={onOpenChange} title={title}>
      <div className="flex flex-col gap-3 text-sm">
        {children}
        <p className="rounded border border-warn/40 bg-warn/10 px-3 py-2 text-ink">{EXTERNAL_RISK_TEXT}</p>
        <label className="flex items-start gap-2 text-ink-muted">
          <input type="checkbox" checked={accepted} onChange={(e) => setAccepted(e.target.checked)} className="mt-1" />I
          understand and accept this risk
        </label>
        <div className="mt-1 flex justify-end gap-2">
          <Button variant="ghost" type="button" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button variant="primary" type="button" disabled={!accepted || busy} onClick={onConfirm}>
            {confirmLabel}
          </Button>
        </div>
      </div>
    </Dialog>
  );
}
