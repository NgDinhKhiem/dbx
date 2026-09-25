import { computed } from "vue";
import { describe, expect, it, vi } from "vitest";

describe("lazy pinyin-pro loading", () => {
  it("keeps the ASCII fast path synchronous and fills in Han initials once loaded", async () => {
    vi.resetModules();
    const pinyin = await import("@/lib/common/pinyin");
    expect(pinyin.isPinyinLoaded()).toBe(false);

    expect(pinyin.pinyinFirstLetters("order_id")).toBe("orderid");
    const initials = computed(() => pinyin.pinyinFirstLetters("总租金a"));
    // Before pinyin-pro arrives Han characters contribute nothing (and are not cached).
    expect(initials.value).toBe("a");

    await pinyin.loadPinyin();
    expect(pinyin.isPinyinLoaded()).toBe(true);
    // Reactive callers recompute once the dictionary is available.
    expect(initials.value).toBe("zzja");
    expect(pinyin.matchesPinyinInitials("总租金", "zzj")).toBe(true);
  });
});
