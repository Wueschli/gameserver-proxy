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
    version: "0.3.0",
    protocol: "1.1",
    skew: "none",
    pools: [
      {
        name: "lobby",
        balancer: "rr",
        backends: [{ addr: "10.0.0.5:7777", healthy: true, state: "enabled", active: 2 }],
      },
    ],
  },
];
const skewed: FleetInstanceView[] = [
  ...instances,
  {
    instance: "ams-1",
    last_seen_ms_ago: 800,
    stale: false,
    sessions: { tcp: 0, udp: 0 },
    version: "0.2.4",
    protocol: "1.0",
    skew: "within-window",
    pools: [],
  },
  {
    instance: "old-1",
    last_seen_ms_ago: 900,
    stale: false,
    sessions: { tcp: 0, udp: 0 },
    version: "0.1.0",
    protocol: "1.0",
    skew: "outside-window",
    pools: [],
  },
];
const shown = vi.hoisted(() => ({ skewed: false }));
vi.mock("../useFleetSocket", () => ({
  useFleetSocket: () => ({
    instances: shown.skewed ? skewed : instances,
    connected: true,
  }),
}));

import { FleetPage } from "./FleetPage";

beforeEach(() => {
  shown.skewed = false;
  Object.values(api).forEach((m) => m.mockReset().mockResolvedValue("ok"));
});

describe("FleetPage component versions", () => {
  it("shows the version and protocol of every instance", () => {
    shown.skewed = true;
    render(<FleetPage />);
    expect(screen.getByText("v0.3.0 · protocol 1.1")).toBeInTheDocument();
    expect(screen.getByText("v0.2.4 · protocol 1.0")).toBeInTheDocument();
  });

  it("flags skew: yellow within the window, red outside it, nothing without skew", () => {
    shown.skewed = true;
    render(<FleetPage />);
    expect(screen.getByText("older, in window")).toBeInTheDocument();
    expect(screen.getByText("outside window")).toBeInTheDocument();
    expect(screen.getAllByText(/in window|outside window/)).toHaveLength(2);
  });
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
