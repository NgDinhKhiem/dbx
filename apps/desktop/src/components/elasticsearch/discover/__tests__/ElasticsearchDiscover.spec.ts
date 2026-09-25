// @vitest-environment happy-dom

import { createApp, defineComponent, h, nextTick, ref, type App } from "vue";
import { createI18n } from "vue-i18n";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import en from "@/i18n/locales/en";
import { HIGHLIGHT_POST_TAG, HIGHLIGHT_PRE_TAG } from "@/lib/elasticsearch/discover/documents";
import type { DiscoverState } from "@/lib/elasticsearch/discover/types";

type RawRequest = { method: string; path: string; body?: string };
type RawResponse = { status: number; body: string; tookMs: number };

const backend = vi.hoisted(() => ({
  elasticsearchRawRequest: vi.fn<(connectionId: string, request: RawRequest) => Promise<RawResponse>>(),
}));

vi.mock("@/lib/backend/api", () => ({ elasticsearchRawRequest: backend.elasticsearchRawRequest }));
vi.mock("vue-echarts", () => ({ default: defineComponent({ name: "VChartStub", setup: () => () => h("div", { "data-testid": "vchart-stub" }) }) }));
vi.mock("@/composables/useTheme", () => ({ useTheme: () => ({ isDark: ref(true) }) }));

import ElasticsearchDiscover from "../ElasticsearchDiscover.vue";

const MAPPING = {
  "logs-local-2026.09.24": {
    mappings: {
      properties: {
        "@timestamp": { type: "date" },
        level: { type: "keyword" },
        severity: { type: "integer" },
        message: { type: "text", fields: { keyword: { type: "keyword" } } },
        service: { properties: { name: { type: "keyword" } } },
      },
    },
  },
};

function hit(id: string, source: Record<string, unknown>, highlight?: Record<string, string[]>) {
  return { _index: "logs-local-2026.09.24", _id: id, _score: null, _source: source, ...(highlight ? { highlight } : {}) };
}

const DEFAULT_HITS = [
  hit("1", { "@timestamp": "2026-09-24T10:00:03.000Z", level: "ERROR", severity: 50, message: "boom", service: { name: "api" } }),
  hit("2", { "@timestamp": "2026-09-24T10:00:02.000Z", level: "ERROR", severity: 50, message: "bang", service: { name: "web" } }),
  hit("3", { "@timestamp": "2026-09-24T10:00:01.000Z", level: "INFO", severity: 30, message: "ok", service: { name: "api" } }),
];

function searchResponse(hits: unknown[] = DEFAULT_HITS, total = hits.length) {
  return {
    took: 7,
    timed_out: false,
    hits: { total: { value: total, relation: "eq" }, max_score: null, hits },
    aggregations: { histogram: { buckets: [{ key: 1758708000000, doc_count: 3 }] } },
  };
}

let searchHandler: (request: RawRequest) => Promise<RawResponse> | RawResponse;

function ok(body: unknown, tookMs = 5): RawResponse {
  return { status: 200, body: JSON.stringify(body), tookMs };
}

function route(_connectionId: string, request: RawRequest): Promise<RawResponse> {
  if (request.path === "/") return Promise.resolve(ok({ version: { distribution: "opensearch", number: "1.3.19" } }));
  if (request.path.endsWith("/_mapping")) return Promise.resolve(ok(MAPPING));
  if (request.path.startsWith("/_cat/indices")) return Promise.resolve(ok([{ index: "logs-local-2026.09.24" }, { index: "logs-local-2026.09.25" }]));
  if (request.path.startsWith("/_cat/aliases")) return Promise.resolve(ok([]));
  if (request.path.endsWith("/_search")) return Promise.resolve(searchHandler(request));
  if (request.path === "/_plugins/_ppl") {
    return Promise.resolve(
      ok({
        schema: [
          { name: "level", type: "string" },
          { name: "c", type: "integer" },
        ],
        datarows: [
          ["ERROR", 2],
          ["INFO", 1],
        ],
        total: 2,
        size: 2,
      }),
    );
  }
  return Promise.resolve({ status: 404, body: "{}", tookMs: 1 });
}

const mounted: Array<{ app: App; container: HTMLElement }> = [];

async function flush(times = 6) {
  for (let i = 0; i < times; i += 1) {
    await Promise.resolve();
    await new Promise((resolve) => setTimeout(resolve, 0));
    await nextTick();
  }
}

function baseState(partial: Partial<DiscoverState> = {}): DiscoverState {
  return { indexPattern: "logs-local-*", timeRange: { from: "now-15m", to: "now" }, language: "dql", query: "", filters: [], columns: [], sort: [], ...partial };
}

async function mountDiscover(initialState?: DiscoverState) {
  const container = document.createElement("div");
  document.body.append(container);
  const stateChanges: DiscoverState[] = [];
  const app = createApp(ElasticsearchDiscover, {
    connectionId: "conn-1",
    initialState,
    onStateChange: (state: DiscoverState) => stateChanges.push(state),
  });
  app.use(createI18n({ legacy: false, locale: "en", messages: { en }, missingWarn: false, fallbackWarn: false }));
  app.mount(container);
  mounted.push({ app, container });
  await flush();
  return { container, stateChanges };
}

function searchCalls(): Array<{ path: string; body: Record<string, any> }> {
  return backend.elasticsearchRawRequest.mock.calls.filter(([, request]) => request.path.endsWith("/_search")).map(([, request]) => ({ path: request.path, body: JSON.parse(request.body ?? "{}") }));
}

function lastSearch() {
  const calls = searchCalls();
  return calls[calls.length - 1];
}

function query(container: HTMLElement, selector: string): HTMLElement {
  const element = container.querySelector<HTMLElement>(selector);
  if (!element) throw new Error(`missing ${selector}`);
  return element;
}

async function typeQuery(container: HTMLElement, text: string) {
  const input = query(container, "[data-testid=discover-query]") as HTMLInputElement;
  input.value = text;
  input.dispatchEvent(new Event("input"));
  await nextTick();
  input.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
  await flush();
}

beforeEach(() => {
  backend.elasticsearchRawRequest.mockReset();
  backend.elasticsearchRawRequest.mockImplementation(route);
  searchHandler = () => ok(searchResponse());
});

afterEach(() => {
  for (const { app, container } of mounted.splice(0)) {
    app.unmount();
    container.remove();
  }
});

describe("ElasticsearchDiscover", () => {
  it("loads the mapping and runs the initial Discover search", async () => {
    const { container } = await mountDiscover(baseState());
    const call = lastSearch();
    expect(call.path).toBe("/logs-local-*/_search");
    expect(call.body).toMatchObject({
      size: 100,
      track_total_hits: true,
      sort: [{ "@timestamp": { order: "desc", unmapped_type: "boolean" } }],
      highlight: { pre_tags: [HIGHLIGHT_PRE_TAG], post_tags: [HIGHLIGHT_POST_TAG] },
      aggs: { histogram: { date_histogram: { field: "@timestamp", fixed_interval: "15s", min_doc_count: 0 } } },
    });
    expect(call.body.query.bool.filter[0].range["@timestamp"]).toMatchObject({ format: "strict_date_optional_time" });
    expect(call.body.query.bool.must).toBeUndefined();
    expect(query(container, "[data-testid=discover-hit-count]").textContent).toContain("3");
    expect(query(container, "[data-testid=discover-took]").textContent).toContain("7");
    expect(container.querySelectorAll("[data-testid=discover-row]")).toHaveLength(3);
    expect(container.querySelector("[data-testid=discover-histogram]")).not.toBeNull();
  });

  it("builds the DQL query when Enter is pressed and emits state", async () => {
    const { container, stateChanges } = await mountDiscover(baseState());
    await typeQuery(container, "level:ERROR and not service.name:web*");
    expect(searchCalls()).toHaveLength(2);
    expect(lastSearch().body.query.bool.must).toEqual([{ bool: { filter: [{ match: { level: "ERROR" } }, { bool: { must_not: { query_string: { fields: ["service.name"], query: "web*" } } } }] } }]);
    expect(stateChanges[stateChanges.length - 1]).toMatchObject({ indexPattern: "logs-local-*", language: "dql", query: "level:ERROR and not service.name:web*" });
  });

  it("shows DQL syntax errors without sending a request", async () => {
    const { container } = await mountDiscover(baseState());
    await typeQuery(container, "level:(ERROR");
    expect(searchCalls()).toHaveLength(1);
    const error = query(container, "[data-testid=discover-dql-error]");
    expect(error.textContent).toContain('Expected ")" but found end of input');
    expect(error.querySelector("pre")?.textContent).toBe("level:(ERROR\n            ^");
  });

  it("shows top values for a clicked field computed from the loaded documents", async () => {
    const { container } = await mountDiscover(baseState());
    query(container, "[data-testid=discover-field-level]").click();
    await nextTick();
    const details = query(container, "[data-testid=discover-field-details-level]");
    const values = [...details.querySelectorAll("[data-testid=discover-top-value]")].map((element) => element.textContent?.replace(/\s+/g, ""));
    expect(values).toEqual(["ERROR66.7%", "INFO33.3%"]);
    expect(details.textContent).toContain("Exists in 3 / 3 loaded records");
  });

  it("adds include/exclude filter pills from top values and re-runs the search", async () => {
    const { container, stateChanges } = await mountDiscover(baseState());
    query(container, "[data-testid=discover-field-level]").click();
    await nextTick();
    const includes = container.querySelectorAll<HTMLElement>("[data-testid=discover-top-value-include]");
    includes[0].click();
    await flush();
    expect(container.querySelector("[data-testid=discover-filter-pill-level]")?.textContent).toContain("ERROR");
    expect(lastSearch().body.query.bool.filter).toContainEqual({ match_phrase: { level: "ERROR" } });

    container.querySelectorAll<HTMLElement>("[data-testid=discover-top-value-exclude]")[1].click();
    await flush();
    expect(lastSearch().body.query.bool.must_not).toEqual([{ match_phrase: { level: "INFO" } }]);
    expect(stateChanges[stateChanges.length - 1].filters.map((filter) => [filter.field, filter.operator, filter.value])).toEqual([
      ["level", "is", "ERROR"],
      ["level", "is_not", "INFO"],
    ]);
  });

  it("adds columns and sorts by a column header", async () => {
    const { container, stateChanges } = await mountDiscover(baseState());
    query(container, "[data-testid=discover-toggle-column-severity]").click();
    await nextTick();
    expect(stateChanges[stateChanges.length - 1].columns).toEqual(["severity"]);
    query(container, "[data-testid=discover-sort-severity]").click();
    await flush();
    expect(lastSearch().body.sort).toEqual([{ severity: { order: "asc", unmapped_type: "boolean" } }]);
    query(container, "[data-testid=discover-sort-severity]").click();
    await flush();
    expect(lastSearch().body.sort).toEqual([{ severity: { order: "desc", unmapped_type: "boolean" } }]);
  });

  it("renders highlights and document values as text, never HTML", async () => {
    searchHandler = () => ok(searchResponse([hit("x", { "@timestamp": "2026-09-24T10:00:00.000Z", message: "<img src=x onerror=alert(1)> boom" }, { message: [`<img src=x onerror=alert(1)> ${HIGHLIGHT_PRE_TAG}boom${HIGHLIGHT_POST_TAG}`] })]));
    const { container } = await mountDiscover(baseState());
    const summary = query(container, "[data-testid=discover-source-summary]");
    expect(summary.querySelector("img")).toBeNull();
    expect(summary.querySelector("mark")?.textContent).toBe("boom");
    expect(summary.textContent).toContain("<img src=x onerror=alert(1)>");
  });

  it("expands a row into table and JSON views", async () => {
    const { container } = await mountDiscover(baseState());
    container.querySelectorAll<HTMLElement>("[data-testid=discover-row]")[0].click();
    await nextTick();
    const detail = query(container, "[data-testid=discover-doc-detail]");
    expect(detail.querySelector("[data-testid='discover-detail-row-service.name']")?.textContent).toContain("api");
    query(detail, "[data-testid=discover-detail-json-tab]").click();
    await nextTick();
    const json = JSON.parse(query(detail, "[data-testid=discover-detail-json]").textContent ?? "{}");
    expect(json).toMatchObject({ _id: "1", _index: "logs-local-2026.09.24", _source: { level: "ERROR" } });
  });

  it("ignores out-of-order responses", async () => {
    const { container } = await mountDiscover(baseState());
    let releaseSlow: (response: RawResponse) => void = () => undefined;
    searchHandler = () => new Promise<RawResponse>((resolve) => (releaseSlow = resolve));
    await typeQuery(container, "level:SLOW");
    searchHandler = () => ok(searchResponse([hit("fast", { "@timestamp": "2026-09-24T10:00:00.000Z", level: "FAST" })], 1));
    await typeQuery(container, "level:FAST");
    releaseSlow(ok(searchResponse([hit("slow-1", { level: "SLOW" }), hit("slow-2", { level: "SLOW" })], 2)));
    await flush();
    expect(container.querySelectorAll("[data-testid=discover-row]")).toHaveLength(1);
    expect(query(container, "[data-testid=discover-hit-count]").textContent).toContain("1");
  });

  it("shows the OpenSearch error reason", async () => {
    searchHandler = () => ({
      status: 400,
      tookMs: 3,
      body: JSON.stringify({
        error: { root_cause: [{ type: "query_shard_exception", reason: "Failed to parse query" }], type: "search_phase_execution_exception", reason: "all shards failed", failed_shards: [{ reason: { type: "query_shard_exception", reason: "Failed to parse query [a:(]" } }] },
        status: 400,
      }),
    });
    const { container } = await mountDiscover(baseState({ language: "lucene", query: "a:(" }));
    expect(lastSearch().body.query.bool.must).toEqual([{ query_string: { query: "a:(", analyze_wildcard: true } }]);
    expect(query(container, "[data-testid=discover-error]").textContent).toContain("query_shard_exception: Failed to parse query [a:(]");
  });

  it("loads more documents up to the sample size", async () => {
    const many = Array.from({ length: 100 }, (_, index) => hit(`h${index}`, { "@timestamp": "2026-09-24T10:00:00.000Z", level: "INFO" }));
    searchHandler = (request) => {
      const body = JSON.parse(request.body ?? "{}");
      const page = body.from ? many.map((entry) => ({ ...entry, _id: `${entry._id}-p${body.from}` })) : many;
      return ok(searchResponse(page, 1000));
    };
    const { container } = await mountDiscover(baseState());
    const table = query(container, "[data-testid=discover-doc-table]");
    // Progressive rendering: only the first rows are in the DOM.
    expect(table.querySelectorAll("[data-testid=discover-row]")).toHaveLength(50);
    [...table.querySelectorAll("button")].find((button) => button.textContent?.includes("Show all"))?.click();
    await nextTick();
    query(container, "[data-testid=discover-load-more]").click();
    await flush();
    expect(lastSearch().body).toMatchObject({ from: 100, size: 100 });
    expect(lastSearch().body.aggs).toBeUndefined();
    expect(table.querySelectorAll("[data-testid=discover-row]").length).toBeGreaterThan(50);
  });

  it("runs PPL with an automatic source and renders the table", async () => {
    const { container } = await mountDiscover(baseState({ language: "ppl", query: "where level='ERROR' | head 5" }));
    expect(searchCalls()).toHaveLength(0);
    expect(query(container, "[data-testid=discover-query]").getAttribute("placeholder")).toContain("where level = 'ERROR' | stats count() by service.name");
    query(container, "[data-testid=discover-refresh]").click();
    await flush();
    const pplCall = backend.elasticsearchRawRequest.mock.calls.find(([, request]) => request.path === "/_plugins/_ppl");
    expect(JSON.parse(pplCall?.[1].body ?? "{}")).toEqual({ query: "source=logs-local-* | where level='ERROR' | head 5" });
    const table = query(container, "[data-testid=discover-tabular-result]");
    expect(table.textContent).toContain("level");
    expect(table.textContent).toContain("ERROR");
  });

  describe("column resizing", () => {
    function drag(handle: HTMLElement, fromX: number, toX: number) {
      handle.dispatchEvent(new MouseEvent("mousedown", { button: 0, clientX: fromX, bubbles: true }));
      document.dispatchEvent(new MouseEvent("mousemove", { clientX: toX, bubbles: true }));
      document.dispatchEvent(new MouseEvent("mouseup", { clientX: toX, bubbles: true }));
    }

    it("drags a column wider, clamps it, and saves the width in the tab state", async () => {
      const { container, stateChanges } = await mountDiscover(baseState({ columns: ["level"] }));
      const searchesBefore = searchCalls().length;
      drag(query(container, "[data-testid=discover-resize-level]"), 100, 400);
      await flush();
      expect(query(container, "[data-testid=discover-col-level]").style.width).toBe("300px");
      expect(stateChanges.at(-1)?.columnWidths).toEqual({ level: 300 });
      // Resizing never sorts or searches.
      expect(searchCalls().length).toBe(searchesBefore);

      drag(query(container, "[data-testid=discover-resize-level]"), 400, -5000);
      await flush();
      expect(stateChanges.at(-1)?.columnWidths).toEqual({ level: 60 });
    });

    it("restores saved widths and resets a column on double-click", async () => {
      const { container, stateChanges } = await mountDiscover(baseState({ columns: ["level"], columnWidths: { level: 250, "\u0000time": 140, bogus: Number.NaN } as Record<string, number> }));
      expect(query(container, "[data-testid=discover-col-level]").style.width).toBe("250px");
      expect(query(container, "[data-testid=discover-col-time]").style.width).toBe("140px");
      query(container, "[data-testid=discover-resize-level]").dispatchEvent(new MouseEvent("dblclick", { bubbles: true }));
      await flush();
      expect(query(container, "[data-testid=discover-col-level]").style.width).toBe("");
      expect(stateChanges.at(-1)?.columnWidths).toEqual({ "\u0000time": 140 });
    });

    it("resizes from the keyboard", async () => {
      const { container, stateChanges } = await mountDiscover(baseState({ columns: ["level"], columnWidths: { level: 200 } }));
      const handle = query(container, "[data-testid=discover-resize-level]");
      handle.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowRight", bubbles: true }));
      await flush();
      expect(stateChanges.at(-1)?.columnWidths).toEqual({ level: 216 });
      handle.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowLeft", shiftKey: true, bubbles: true }));
      await flush();
      expect(stateChanges.at(-1)?.columnWidths).toEqual({ level: 152 });
    });
  });
});
