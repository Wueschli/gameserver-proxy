import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

const api = vi.hoisted(() => ({
  getCurrentConfig: vi.fn(),
  submitConfig: vi.fn(),
}));
vi.mock("../api", async (orig) => ({ ...(await orig<typeof import("../api")>()), ...api }));

import { SettingsPage } from "./SettingsPage";

beforeEach(() => {
  api.getCurrentConfig.mockReset().mockResolvedValue({ text: "settings:\n  workers: 2\n", revision: "3" });
  api.submitConfig.mockReset().mockResolvedValue({ revision: 4 });
});

describe("SettingsPage submit", () => {
  it("asks first and only submits once confirmed", async () => {
    render(<SettingsPage />);
    await userEvent.click(await screen.findByRole("button", { name: "Submit as a new revision" }));
    expect(api.submitConfig).not.toHaveBeenCalled();
    const dlg = screen.getByRole("dialog");
    expect(dlg).toHaveTextContent(/every instance/i);
    await userEvent.click(within(dlg).getByRole("button", { name: "Apply" }));
    expect(api.submitConfig).toHaveBeenCalledWith("settings:\n  workers: 2\n");
    expect(await screen.findByText(/accepted as revision 4/)).toBeInTheDocument();
  });

  it("does not submit when cancelled", async () => {
    render(<SettingsPage />);
    await userEvent.click(await screen.findByRole("button", { name: "Submit as a new revision" }));
    await userEvent.click(within(screen.getByRole("dialog")).getByRole("button", { name: "Cancel" }));
    expect(api.submitConfig).not.toHaveBeenCalled();
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });
});
