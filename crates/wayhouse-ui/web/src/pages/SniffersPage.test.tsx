import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

const api = vi.hoisted(() => ({
  deleteSniffer: vi.fn(),
  listInstanceSniffers: vi.fn(),
  uploadSniffer: vi.fn(),
  listRegistries: vi.fn(),
  listRegistrySniffers: vi.fn(),
}));
vi.mock("../api", async (orig) => ({ ...(await orig<typeof import("../api")>()), ...api }));
vi.mock("../useFleetSocket", () => ({
  useFleetSocket: () => ({
    instances: [{ instance: "fra-1", last_seen_ms_ago: 0, stale: false, pools: [], sessions: { tcp: 0, udp: 0 } }],
    connected: true,
  }),
}));

import { SniffersPage } from "./SniffersPage";

beforeEach(() => {
  api.listInstanceSniffers
    .mockReset()
    .mockResolvedValue([{ name: "a2s.wasm", sha256: "ab".repeat(32), size_bytes: 100, loaded: true }]);
  api.listRegistries.mockReset().mockResolvedValue({ registries: [], persistent: true });
  api.deleteSniffer.mockReset().mockResolvedValue({ results: [] });
});

describe("SniffersPage remove", () => {
  it("asks first, warns it is fleet-wide, and only then deletes", async () => {
    render(<SniffersPage />);
    await userEvent.click(await screen.findByRole("button", { name: "remove" }));
    expect(api.deleteSniffer).not.toHaveBeenCalled();
    const dlg = screen.getByRole("dialog");
    expect(dlg).toHaveTextContent("a2s.wasm");
    expect(dlg).toHaveTextContent(/every instance/i);
    await userEvent.click(within(dlg).getByRole("button", { name: "Remove" }));
    expect(api.deleteSniffer).toHaveBeenCalledWith("a2s.wasm");
  });
});
