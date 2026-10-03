import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { ApiError } from "../api";

const api = vi.hoisted(() => ({
  getTunnelAddresses: vi.fn(),
}));
vi.mock("../api", async (orig) => ({ ...(await orig<typeof import("../api")>()), ...api }));

import { TunnelAddressesPage } from "./TunnelAddressesPage";

const NOW = 1_800_000_000;

beforeEach(() => {
  vi.spyOn(Date, "now").mockReturnValue(NOW * 1000);
  api.getTunnelAddresses.mockReset().mockResolvedValue({
    network: "10.200.0.0/24",
    allocated: 2,
    capacity: 254,
    entries: [
      {
        role: "origin",
        name: "fra-origin",
        address: "10.200.0.2",
        first_seen: NOW - 86_400,
        last_seen: NOW - 120,
        stale: false,
      },
      {
        role: "proxy",
        name: "edge-1",
        address: "10.200.0.3",
        first_seen: NOW - 40 * 86_400,
        last_seen: NOW - 20 * 86_400,
        stale: true,
      },
    ],
  });
});

describe("TunnelAddressesPage", () => {
  it("shows the network, its usage and one row per owner", async () => {
    render(<TunnelAddressesPage />);
    expect(await screen.findByText("10.200.0.0/24")).toBeInTheDocument();
    expect(screen.getByText(/2 of 254 allocated/)).toBeInTheDocument();

    const origin = screen.getByText("fra-origin").closest("tr")!;
    expect(within(origin).getByText("origin")).toBeInTheDocument();
    expect(within(origin).getByText("10.200.0.2")).toBeInTheDocument();
    expect(within(origin).getByText("2 min ago")).toBeInTheDocument();
    expect(within(origin).getByText("active")).toBeInTheDocument();

    const proxy = screen.getByText("edge-1").closest("tr")!;
    expect(within(proxy).getByText("proxy")).toBeInTheDocument();
    expect(within(proxy).getByText("20 d ago")).toBeInTheDocument();
    expect(within(proxy).getByText("stale")).toBeInTheDocument();
  });

  it("explains how to free a stale address when one is listed", async () => {
    render(<TunnelAddressesPage />);
    expect(await screen.findByText(/1 stale/)).toBeInTheDocument();
    expect(screen.getByText(/DELETE \/peers\/\{name\}/)).toBeInTheDocument();
  });

  it("says when the controller has no tunnel network", async () => {
    api.getTunnelAddresses.mockResolvedValue({
      network: null,
      allocated: 0,
      capacity: null,
      entries: [],
    });
    render(<TunnelAddressesPage />);
    expect(await screen.findByText(/no --tunnel-network/)).toBeInTheDocument();
    expect(screen.getByText("No tunnel addresses assigned yet.")).toBeInTheDocument();
  });

  it("shows the controller's error instead of a table", async () => {
    api.getTunnelAddresses.mockRejectedValue(
      new ApiError(503, "no controller configured (--controller-url)"),
    );
    render(<TunnelAddressesPage />);
    expect(await screen.findByText("no controller configured (--controller-url)")).toBeInTheDocument();
    expect(screen.queryByRole("table")).not.toBeInTheDocument();
  });

  it("refresh fetches the table again", async () => {
    render(<TunnelAddressesPage />);
    await screen.findByText("fra-origin");
    await userEvent.click(screen.getByRole("button", { name: "Refresh" }));
    expect(api.getTunnelAddresses).toHaveBeenCalledTimes(2);
  });
});
