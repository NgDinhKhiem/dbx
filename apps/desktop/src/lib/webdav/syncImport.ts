import type { SyncImportApplySummary, SyncImportConfirmation, SyncImportReview, SyncImportStatus } from "@/lib/backend/api";

/** Minimum length for newly entered sync-secrets and new snippet encryption passphrases. */
export const SYNC_PASSPHRASE_MIN_LENGTH = 12;

export function isSyncPassphraseTooShort(passphrase: string | null | undefined): boolean {
  const value = passphrase?.trim() ?? "";
  return value.length > 0 && value.length < SYNC_PASSPHRASE_MIN_LENGTH;
}

const LOOPBACK_HOSTS = new Set(["localhost", "::1", "[::1]"]);

function isLoopbackHost(hostname: string): boolean {
  const host = hostname.toLowerCase().replace(/\.$/, "");
  if (LOOPBACK_HOSTS.has(host) || host.endsWith(".localhost")) return true;
  const octets = host.split(".");
  return octets.length === 4 && octets[0] === "127" && octets.every((part) => /^\d{1,3}$/.test(part) && Number(part) <= 255);
}

/**
 * Sync endpoints (WebDAV, self-hosted GitLab) must use https://, except for
 * loopback hosts (localhost, 127.0.0.0/8, ::1). Returns true for a parseable URL
 * the backend would reject with HTTPS_REQUIRED; unparseable input is left to
 * other validation.
 */
export function isInsecureSyncUrl(value: string | null | undefined): boolean {
  const raw = value?.trim() ?? "";
  if (!raw) return false;
  let url: URL;
  try {
    url = new URL(raw);
  } catch {
    return false;
  }
  if (url.protocol === "https:") return false;
  if (url.protocol !== "http:") return true;
  return !isLoopbackHost(url.hostname);
}

export interface SyncDownloadResultLike {
  status: SyncImportStatus;
  review?: SyncImportReview;
  applySummary?: SyncImportApplySummary;
}

/**
 * Resolves the user's decision for a review: true keeps local saved
 * credentials of changed connections, false clears them, null cancels.
 */
export type SyncImportConfirmer = (review: SyncImportReview) => Promise<boolean | null>;

/**
 * Runs a cloud-sync download, asking the user to confirm whenever the backend
 * reports `confirmationRequired` (nothing is applied in that case) and
 * retrying with the confirmation token. The backend issues a fresh review when
 * the remote snapshot changed in between, so the loop asks again. Returns null
 * when the user cancels.
 */
export async function runConfirmedSyncDownload<R extends SyncDownloadResultLike>(download: (confirmation: SyncImportConfirmation | null) => Promise<R>, confirm: SyncImportConfirmer): Promise<R | null> {
  let result = await download(null);
  while (result.status === "confirmationRequired") {
    const review = result.review;
    if (!review) throw new Error("Sync import requires confirmation but no review was returned.");
    const keepLocalSecrets = await confirm(review);
    if (keepLocalSecrets === null) return null;
    result = await download({ token: review.token, keepLocalSecrets });
  }
  return result;
}
