// @vitest-environment happy-dom

import { createApp, defineComponent, h, nextTick, reactive, type App } from "vue";
import { afterEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import SyncImportConfirmDialog from "@/components/sync/SyncImportConfirmDialog.vue";
import type { SyncImportReview } from "@/lib/backend/api";

const mountedApps: App[] = [];

async function mountDialog(review: SyncImportReview, listeners: { onConfirm?: (keep: boolean) => void; onCancel?: () => void } = {}) {
  const state = reactive({ open: true });
  const container = document.createElement("div");
  document.body.append(container);
  const app = createApp(
    defineComponent({
      setup: () => () =>
        h(SyncImportConfirmDialog, {
          open: state.open,
          review,
          ...listeners,
          "onUpdate:open": (value: boolean) => {
            state.open = value;
          },
        }),
    }),
  );
  mountedApps.push(app);
  app.use(i18n);
  app.mount(container);
  await nextTick();
  await new Promise((resolve) => setTimeout(resolve, 0));
  return state;
}

afterEach(() => {
  for (const app of mountedApps.splice(0)) app.unmount();
  document.body.innerHTML = "";
});

const changedReview: SyncImportReview = {
  authenticated: false,
  token: "token-1",
  endpointChanges: [
    {
      connectionId: "c1",
      connectionName: "Production",
      changes: [{ field: "host", before: "db.internal", after: "db.attacker.example" }],
    },
  ],
};

describe("SyncImportConfirmDialog", () => {
  it("warns about unauthenticated snapshots and lists endpoint changes", async () => {
    await mountDialog(changedReview);

    expect(document.body.querySelector("[data-sync-import-unauthenticated]")?.textContent).toContain("not authenticated");
    const changes = document.body.querySelector("[data-sync-import-endpoint-changes]")?.textContent ?? "";
    expect(changes).toContain("Production");
    expect(changes).toContain("host");
    expect(changes).toContain("db.internal");
    expect(changes).toContain("db.attacker.example");
    expect(document.body.querySelector("[data-sync-import-clear]")).not.toBeNull();
  });

  it("emits the credential choice", async () => {
    const onConfirm = vi.fn();
    const state = await mountDialog(changedReview, { onConfirm });

    (document.body.querySelector("[data-sync-import-clear]") as HTMLButtonElement).click();
    await nextTick();

    expect(onConfirm).toHaveBeenCalledWith(false);
    expect(state.open).toBe(false);
  });

  it("offers only keep and cancel without endpoint changes", async () => {
    const onConfirm = vi.fn();
    const onCancel = vi.fn();
    await mountDialog({ authenticated: false, token: "t", endpointChanges: [] }, { onConfirm, onCancel });

    expect(document.body.querySelector("[data-sync-import-clear]")).toBeNull();
    (document.body.querySelector("[data-sync-import-cancel]") as HTMLButtonElement).click();
    await nextTick();
    expect(onCancel).toHaveBeenCalledTimes(1);
    expect(onConfirm).not.toHaveBeenCalled();
  });

  it("hides the authentication warning for authenticated snapshots", async () => {
    const onConfirm = vi.fn();
    await mountDialog({ ...changedReview, authenticated: true }, { onConfirm });

    expect(document.body.querySelector("[data-sync-import-unauthenticated]")).toBeNull();
    (document.body.querySelector("[data-sync-import-keep]") as HTMLButtonElement).click();
    await nextTick();
    expect(onConfirm).toHaveBeenCalledWith(true);
  });
});
