import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

const api = vi.hoisted(() => ({
  deleteSniffer: vi.fn(),
  listInstanceSniffers: vi.fn(),
  uploadSniffer: vi.fn(),
  listRegistries: vi.fn(),
  listRegistrySniffers: vi.fn(),
  checkUpdates: vi.fn(),
  rollbackSniffer: vi.fn(),
  installFromRegistry: vi.fn(),
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
  api.checkUpdates.mockReset().mockResolvedValue(checked([]));
  api.rollbackSniffer.mockReset().mockResolvedValue({ results: [] });
  api.installFromRegistry.mockReset();
});

const OFFICIAL = {
    id: "aaaaaaaaaaaa",
    name: "official",
    url: "https://o.example/index.json",
    official: true,
    risk: "official",
};

function checked(sniffers: object[]) {
    return {
        host_abi: "0.1",
        min_proxy_checked: false,
        registries: [],
        sniffers,
        instance_errors: [],
    };
}

function row(extra: object = {}) {
    return {
        sniffer: "demo",
        installed_sha256: "cd".repeat(32),
        installed_version: "0.1.0",
        known: true,
        has_previous: false,
        fallback: false,
        update: { registry_id: OFFICIAL.id, version: "0.2.0", compatible: true, reason: null },
        instances: ["fra-1"],
        ...extra,
    };
}

function installable() {
    const version = (v: string) => ({
        version: v,
        abi: "0.1",
        min_proxy: "0.1.0",
        url: "https://o.example/demo.wasm",
        sha256: "ee".repeat(32),
        size: 2048,
        limits: { max_memory_bytes: 16777216, call_timeout_ms: 20 },
    });
    return {
        registry: OFFICIAL,
        index_name: "official",
        host_abi: "0.1",
        min_proxy_checked: false,
        sniffers: [
            {
                name: "demo",
                description: "Detects demo traffic",
                license: "MIT",
                versions: [version("0.2.0"), version("0.1.0")],
                compatible: { version: "0.2.0" },
            },
        ],
    };
}


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

describe("SniffersPage updates and rollback", () => {
    it("calls the check route once on click and never polls", async () => {
        vi.useFakeTimers({ shouldAdvanceTime: true });
        try {
            render(<SniffersPage />);
            await screen.findByRole("button", { name: "remove" });
            expect(api.checkUpdates).not.toHaveBeenCalled();
            await userEvent.click(screen.getByRole("button", { name: "Check for updates" }));
            expect(api.checkUpdates).toHaveBeenCalledTimes(1);
            await vi.advanceTimersByTimeAsync(10 * 60 * 1000);
            expect(api.checkUpdates).toHaveBeenCalledTimes(1);
        } finally {
            vi.useRealTimers();
        }
    });

    it("shows the newer version and opens the install dialog for it", async () => {
        api.checkUpdates.mockResolvedValue(checked([row()]));
        api.listRegistries.mockResolvedValue({ registries: [OFFICIAL], persistent: true });
        api.listRegistrySniffers.mockResolvedValue(installable());
        api.installFromRegistry.mockResolvedValue({
            sniffer: "demo",
            version: "0.2.0",
            signed: false,
            risk: "official",
            sha256: "ee".repeat(32),
            results: [{ instance: "fra-1", ok: true, pinned: false, error: null, status: 200 }],
            pinned_instances: [],
        });
        render(<SniffersPage />);
        await userEvent.click(await screen.findByRole("button", { name: "Check for updates" }));
        const updates = await screen.findByRole("region", { name: "Updates" });
        expect(updates).toHaveTextContent("0.1.0");
        await userEvent.click(within(updates).getByRole("button", { name: "Update to 0.2.0" }));
        const dlg = await screen.findByRole("dialog");
        expect(dlg).toHaveTextContent("demo");
        expect(dlg).toHaveTextContent("0.2.0");
        await userEvent.click(within(dlg).getByRole("button", { name: /on every instance/i }));
        expect(api.installFromRegistry).toHaveBeenCalledWith(OFFICIAL.id, "demo", "0.2.0");
    });

    it("offers a rollback only for modules that keep a previous version", async () => {
        api.listInstanceSniffers.mockResolvedValue([
            {
                name: "kept",
                sha256: "ab".repeat(32),
                size_bytes: 1,
                loaded: true,
                has_previous: true,
                fallback: false,
            },
            {
                name: "fresh",
                sha256: "ab".repeat(32),
                size_bytes: 1,
                loaded: true,
                has_previous: false,
                fallback: false,
            },
        ]);
        render(<SniffersPage />);
        const rollbacks = await screen.findAllByRole("button", { name: "roll back" });
        expect(rollbacks).toHaveLength(1);
        await userEvent.click(rollbacks[0]);
        expect(api.rollbackSniffer).not.toHaveBeenCalled();
        const dlg = screen.getByRole("dialog");
        expect(dlg).toHaveTextContent("kept");
        expect(dlg).toHaveTextContent(/every instance/i);
        await userEvent.click(within(dlg).getByRole("button", { name: "Roll back" }));
        expect(api.rollbackSniffer).toHaveBeenCalledWith("kept");
    });

    it("shows when a module runs its previous version because the new file failed", async () => {
        api.listInstanceSniffers.mockResolvedValue([
            {
                name: "bad",
                sha256: "ab".repeat(32),
                size_bytes: 1,
                loaded: true,
                has_previous: true,
                fallback: true,
            },
        ]);
        render(<SniffersPage />);
        expect(await screen.findByText("fallback active")).toBeInTheDocument();
    });

    it("gives an unknown build no update button", async () => {
        api.checkUpdates.mockResolvedValue(
            checked([row({ known: false, installed_version: null, update: null })]),
        );
        render(<SniffersPage />);
        await userEvent.click(await screen.findByRole("button", { name: "Check for updates" }));
        const updates = await screen.findByRole("region", { name: "Updates" });
        expect(updates).toHaveTextContent(/unknown build/i);
        expect(within(updates).queryByRole("button", { name: /^Update to/ })).toBeNull();
    });
});
