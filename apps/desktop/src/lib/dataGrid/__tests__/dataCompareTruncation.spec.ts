import { describe, expect, it } from "vitest";
import { dataCompareTruncationNotice } from "@/lib/dataGrid/dataCompare";

const base = { sourceTruncated: false, targetTruncated: false, targetRowCount: 10, added: 0, removed: 0, modified: 0, preSyncStatements: [] as string[] };

describe("dataCompareTruncationNotice", () => {
  it("is silent for complete compares", () => {
    expect(dataCompareTruncationNotice(base)).toBeUndefined();
  });

  it("flags a missing target that only received CREATE TABLE", () => {
    expect(dataCompareTruncationNotice({ ...base, sourceTruncated: true, targetRowCount: 0, preSyncStatements: ["CREATE TABLE t (id int)"] })).toBe("missingTargetTooLarge");
  });

  it("flags capped full compares on either side as partial", () => {
    expect(dataCompareTruncationNotice({ ...base, sourceTruncated: true, added: 3 })).toBe("partial");
    expect(dataCompareTruncationNotice({ ...base, targetTruncated: true })).toBe("partial");
  });
});
