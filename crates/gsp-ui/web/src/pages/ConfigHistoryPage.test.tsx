import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

const api = vi.hoisted(() => ({
  listRevisions: vi.fn(),
  diffRevision: vi.fn(),
  rollbackTo: vi.fn(),
}));
vi.mock("../api", async (orig) => ({ ...(await orig<typeof import("../api")>()), ...api }));

import { ConfigHistoryPage } from "./ConfigHistoryPage";

beforeEach(() => {
  api.listRevisions.mockReset().mockResolvedValue([
    { revision: 2, size_bytes: 10, current: true },
    { revision: 1, size_bytes: 8, current: false },
  ]);
  api.rollbackTo.mockReset().mockResolvedValue({ revision: 3 });
});

describe("ConfigHistoryPage rollback", () => {
  it("asks first, naming the revision, and only then rolls back", async () => {
    render(<ConfigHistoryPage />);
    await userEvent.click(await screen.findByRole("button", { name: "roll back to this" }));
    expect(api.rollbackTo).not.toHaveBeenCalled();
    const dlg = screen.getByRole("dialog");
    expect(dlg).toHaveTextContent("revision 1");
    await userEvent.click(within(dlg).getByRole("button", { name: "Roll back" }));
    expect(api.rollbackTo).toHaveBeenCalledWith(1);
    expect(await screen.findByText(/as new revision 3/)).toBeInTheDocument();
  });

  it("cancelling leaves the config untouched", async () => {
    render(<ConfigHistoryPage />);
    await userEvent.click(await screen.findByRole("button", { name: "roll back to this" }));
    await userEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(api.rollbackTo).not.toHaveBeenCalled();
  });
});
