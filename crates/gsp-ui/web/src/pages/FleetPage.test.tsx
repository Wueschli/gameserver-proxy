import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { FleetInstanceView } from "../types";

const api = vi.hoisted(() => ({
  drainInstance: vi.fn(),
  undrainInstance: vi.fn(),
  deleteBackend: vi.fn(),
  patchBackend: vi.fn(),
  addBackend: vi.fn(),
  routeHint: vi.fn(),
}));
vi.mock("../api", async (orig) => ({ ...(await orig<typeof import("../api")>()), ...api }));

const instances: FleetInstanceView[] = [
  {
    instance: "fra-1",
    last_seen_ms_ago: 1200,
    stale: false,
    sessions: { tcp: 3, udp: 4 },
    pools: [
      {
        name: "lobby",
        balancer: "rr",
        backends: [{ addr: "10.0.0.5:7777", healthy: true, state: "enabled", active: 2 }],
      },
    ],
  },
];
vi.mock("../useFleetSocket", () => ({
  useFleetSocket: () => ({ instances, connected: true }),
}));

import { FleetPage } from "./FleetPage";

beforeEach(() => {
  Object.values(api).forEach((m) => m.mockReset().mockResolvedValue("ok"));
});

describe("FleetPage destructive actions ask first", () => {
  it("Drain does not call the API until confirmed", async () => {
    render(<FleetPage />);
    await userEvent.click(screen.getByRole("button", { name: "Drain" }));
    expect(api.drainInstance).not.toHaveBeenCalled();
    const dlg = screen.getByRole("dialog");
    expect(dlg).toHaveTextContent("fra-1");
    await userEvent.click(within(dlg).getByRole("button", { name: "Drain" }));
    expect(api.drainInstance).toHaveBeenCalledWith("fra-1");
  });

  it("cancelling Drain never calls the API", async () => {
    render(<FleetPage />);
    await userEvent.click(screen.getByRole("button", { name: "Drain" }));
    await userEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(api.drainInstance).not.toHaveBeenCalled();
  });

  it("Undrain is restorative and needs no confirmation", async () => {
    render(<FleetPage />);
    await userEvent.click(screen.getByRole("button", { name: "Undrain" }));
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(api.undrainInstance).toHaveBeenCalledWith("fra-1");
  });

  it("remove backend warns it is fleet-wide and waits for confirmation", async () => {
    render(<FleetPage />);
    await userEvent.click(screen.getByRole("button", { name: "remove" }));
    expect(api.deleteBackend).not.toHaveBeenCalled();
    const dlg = screen.getByRole("dialog");
    expect(dlg).toHaveTextContent("10.0.0.5:7777");
    expect(dlg).toHaveTextContent(/every instance/i);
    await userEvent.click(within(dlg).getByRole("button", { name: "Remove" }));
    expect(api.deleteBackend).toHaveBeenCalledWith("lobby", "10.0.0.5:7777");
  });

  it("moving a backend to disabled needs confirmation; back to enabled does not", async () => {
    render(<FleetPage />);
    const select = screen.getByRole("combobox");
    await userEvent.selectOptions(select, "disabled");
    expect(api.patchBackend).not.toHaveBeenCalled();
    await userEvent.click(within(screen.getByRole("dialog")).getByRole("button", { name: "Disable" }));
    expect(api.patchBackend).toHaveBeenCalledWith("lobby", "10.0.0.5:7777", "disabled");
  });

  it("re-enabling a backend is restorative and needs no confirmation", async () => {
    instances[0].pools[0].backends[0].state = "disabled";
    render(<FleetPage />);
    await userEvent.selectOptions(screen.getByRole("combobox"), "enabled");
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(api.patchBackend).toHaveBeenCalledWith("lobby", "10.0.0.5:7777", "enabled");
    instances[0].pools[0].backends[0].state = "enabled";
  });
});

describe("FleetPage pending and error states", () => {
  it("disables action buttons while a request is in flight, then reports the failure", async () => {
    let reject!: (e: Error) => void;
    api.undrainInstance.mockReturnValue(new Promise((_, rej) => (reject = rej)));
    render(<FleetPage />);
    await userEvent.click(screen.getByRole("button", { name: "Undrain" }));
    expect(screen.getByRole("button", { name: "Undrain" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Drain" })).toBeDisabled();
    reject(new Error("aggregator unreachable"));
    expect(await screen.findByText(/undrain fra-1 failed: .*aggregator unreachable/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Undrain" })).toBeEnabled();
  });
});
