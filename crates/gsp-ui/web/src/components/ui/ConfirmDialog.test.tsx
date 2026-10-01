import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { useConfirm } from "./ConfirmDialog";

function Harness({ onResult }: { onResult: (ok: boolean) => void }) {
  const { confirm, dialog } = useConfirm();
  return (
    <>
      <button
        onClick={async () =>
          onResult(
            await confirm({
              title: "Drain frankfurt-1?",
              description: "New sessions stop.",
              confirmLabel: "Drain",
            }),
          )
        }
      >
        go
      </button>
      {dialog}
    </>
  );
}

describe("useConfirm", () => {
  it("shows nothing until asked", () => {
    render(<Harness onResult={() => {}} />);
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("resolves true when confirmed", async () => {
    const results: boolean[] = [];
    render(<Harness onResult={(ok) => results.push(ok)} />);
    await userEvent.click(screen.getByText("go"));
    expect(screen.getByRole("dialog")).toHaveTextContent("Drain frankfurt-1?");
    expect(screen.getByRole("dialog")).toHaveTextContent("New sessions stop.");
    await userEvent.click(screen.getByRole("button", { name: "Drain" }));
    expect(results).toEqual([true]);
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("resolves false when cancelled", async () => {
    const results: boolean[] = [];
    render(<Harness onResult={(ok) => results.push(ok)} />);
    await userEvent.click(screen.getByText("go"));
    await userEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(results).toEqual([false]);
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("resolves false when dismissed with Escape", async () => {
    const results: boolean[] = [];
    render(<Harness onResult={(ok) => results.push(ok)} />);
    await userEvent.click(screen.getByText("go"));
    await userEvent.keyboard("{Escape}");
    expect(results).toEqual([false]);
  });
});
