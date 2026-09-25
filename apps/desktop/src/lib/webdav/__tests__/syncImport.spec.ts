import { describe, expect, it, vi } from "vitest";
import { isInsecureSyncUrl, isSyncPassphraseTooShort, runConfirmedSyncDownload, SYNC_PASSPHRASE_MIN_LENGTH, type SyncDownloadResultLike } from "@/lib/webdav/syncImport";
import type { SyncImportConfirmation, SyncImportReview } from "@/lib/backend/api";

function review(token: string, authenticated = true): SyncImportReview {
  return {
    authenticated,
    token,
    endpointChanges: [{ connectionId: "c1", connectionName: "prod", changes: [{ field: "host", before: "db.internal", after: "db.attacker.example" }] }],
  };
}

type Result = SyncDownloadResultLike & { label: string };

describe("runConfirmedSyncDownload", () => {
  it("returns an applied result without asking", async () => {
    const download = vi.fn(async (): Promise<Result> => ({ status: "applied", label: "done" }));
    const confirm = vi.fn();

    await expect(runConfirmedSyncDownload(download, confirm)).resolves.toEqual({ status: "applied", label: "done" });
    expect(download).toHaveBeenCalledWith(null);
    expect(confirm).not.toHaveBeenCalled();
  });

  it("retries with the review token and the user's credential choice", async () => {
    const download = vi.fn(async (confirmation: SyncImportConfirmation | null): Promise<Result> => (confirmation ? { status: "applied", label: "applied" } : { status: "confirmationRequired", review: review("t1", false), label: "review" }));
    const confirm = vi.fn(async () => false);

    await expect(runConfirmedSyncDownload(download, confirm)).resolves.toMatchObject({ status: "applied" });
    expect(confirm).toHaveBeenCalledWith(review("t1", false));
    expect(download).toHaveBeenLastCalledWith({ token: "t1", keepLocalSecrets: false });
  });

  it("asks again when the remote snapshot changed and a new token is issued", async () => {
    const download = vi
      .fn<(confirmation: SyncImportConfirmation | null) => Promise<Result>>()
      .mockResolvedValueOnce({ status: "confirmationRequired", review: review("t1"), label: "first" })
      .mockResolvedValueOnce({ status: "confirmationRequired", review: review("t2"), label: "second" })
      .mockResolvedValueOnce({ status: "applied", label: "applied" });
    const confirm = vi.fn(async () => true);

    await expect(runConfirmedSyncDownload(download, confirm)).resolves.toMatchObject({ label: "applied" });
    expect(confirm).toHaveBeenCalledTimes(2);
    expect(download.mock.calls.map(([confirmation]) => confirmation)).toEqual([null, { token: "t1", keepLocalSecrets: true }, { token: "t2", keepLocalSecrets: true }]);
  });

  it("stops without applying when the user cancels", async () => {
    const download = vi.fn(async (): Promise<Result> => ({ status: "confirmationRequired", review: review("t1"), label: "review" }));

    await expect(runConfirmedSyncDownload(download, async () => null)).resolves.toBeNull();
    expect(download).toHaveBeenCalledTimes(1);
  });
});

describe("isInsecureSyncUrl", () => {
  it("requires https except for loopback hosts", () => {
    expect(isInsecureSyncUrl("https://dav.example.com/remote.php/dav")).toBe(false);
    expect(isInsecureSyncUrl("http://dav.example.com/remote.php/dav")).toBe(true);
    expect(isInsecureSyncUrl("http://localhost:8080/dav")).toBe(false);
    expect(isInsecureSyncUrl("http://127.1.2.3/dav")).toBe(false);
    expect(isInsecureSyncUrl("http://[::1]:8080/dav")).toBe(false);
    expect(isInsecureSyncUrl("http://127.0.0.1.example.com/dav")).toBe(true);
    expect(isInsecureSyncUrl("")).toBe(false);
    expect(isInsecureSyncUrl("not a url")).toBe(false);
  });
});

describe("isSyncPassphraseTooShort", () => {
  it("flags only non-empty passphrases below the minimum", () => {
    expect(isSyncPassphraseTooShort("")).toBe(false);
    expect(isSyncPassphraseTooShort("short")).toBe(true);
    expect(isSyncPassphraseTooShort("x".repeat(SYNC_PASSPHRASE_MIN_LENGTH))).toBe(false);
  });
});
