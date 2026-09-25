import { readFileSync } from "node:fs";
import { expect, it } from "vitest";

type ScopedPermission = { identifier: string; allow?: { path: string }[]; deny?: { path: string }[] };
const capability = JSON.parse(readFileSync(new URL("../../src-tauri/capabilities/default.json", import.meta.url), "utf8")) as {
  permissions: (string | ScopedPermission)[];
};

it("grants only exists and readTextFile command scopes for the exact UI tuning file", () => {
  const scopes = capability.permissions.filter((permission): permission is ScopedPermission => typeof permission !== "string" && permission.identifier.startsWith("fs:allow-"));
  expect(scopes).toEqual([
    { identifier: "fs:allow-exists", allow: [{ path: "$HOME/.dbx/ui-tuning.json" }] },
    { identifier: "fs:allow-read-text-file", allow: [{ path: "$HOME/.dbx/ui-tuning.json" }] },
  ]);
  expect(capability.permissions.filter((permission) => typeof permission === "string" && permission.startsWith("fs:"))).toEqual(["fs:deny-default", "fs:allow-write-file", "fs:allow-write-text-file", "fs:allow-stat", "fs:allow-read-file", "fs:allow-open", "fs:allow-read"]);
});

it("keeps the webview out of the app's own data directories (installed plugins, storage)", () => {
  expect(capability.permissions).not.toContain("fs:default");
  const scope = capability.permissions.find((permission): permission is ScopedPermission => typeof permission !== "string" && permission.identifier === "fs:scope");
  expect(scope?.allow).toBeUndefined();
  expect(scope?.deny?.map((entry) => entry.path)).toEqual(expect.arrayContaining(["$APPDATA/**", "$APPLOCALDATA/**", "$APPCONFIG/**"]));
});
