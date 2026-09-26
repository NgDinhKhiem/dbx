//! Pure request building and response shaping for the OpenSearch /
//! Elasticsearch MCP tools: time windows (date math), `_search` bodies,
//! PPL window insertion, SQL validation, saved Dashboards index patterns,
//! `_field_caps` / `_resolve/index` / `_cat` parsing and compact, size-capped
//! responses. No I/O here: callers send the requests through a connection.

use chrono::{DateTime, Datelike, Duration, Months, NaiveDate, NaiveDateTime, TimeZone, Timelike, Utc};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde_json::{json, Map, Value};

use super::elasticsearch_dql::{dql_to_dsl, parse_dql, DqlKnownField, DqlSyntaxError};
use super::elasticsearch_driver::{ppl_query_is_read_only, strip_leading_sql_comments};

/// Default and maximum number of hits / rows a tool returns.
pub const DEFAULT_LIMIT: usize = 20;
pub const MAX_LIMIT: usize = 200;
/// Serialized tool responses are capped at this many bytes.
pub const MAX_RESPONSE_BYTES: usize = 200 * 1024;
/// String values in `_source` / rows longer than this are truncated.
pub const MAX_VALUE_CHARS: usize = 2_000;
pub const DEFAULT_TIME_FIELD: &str = "@timestamp";
pub const DEFAULT_WINDOW_FROM: &str = "now-1h";
pub const DEFAULT_WINDOW_TO: &str = "now";
/// Saved objects index of OpenSearch Dashboards / Kibana (plus versioned and tenant indices).
pub const DASHBOARDS_SAVED_OBJECTS_PATTERN: &str = ".kibana,.kibana_*";

pub fn clamp_limit(limit: Option<u64>) -> usize {
    limit.map_or(DEFAULT_LIMIT, |limit| (limit as usize).clamp(1, MAX_LIMIT))
}

// ---------------------------------------------------------------------------
// Time windows
// ---------------------------------------------------------------------------

/// An absolute, inclusive UTC time window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeWindow {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
}

impl TimeWindow {
    pub fn from_iso(&self) -> String {
        iso_millis(self.from)
    }

    pub fn to_iso(&self) -> String {
        iso_millis(self.to)
    }

    /// `yyyy-MM-dd HH:mm:ss` bounds for OpenSearch SQL/PPL literals. The end
    /// is rounded up to the next whole second so the window stays inclusive.
    pub fn ppl_bounds(&self) -> (String, String) {
        let to = if self.to.nanosecond() > 0 {
            self.to.with_nanosecond(0).unwrap_or(self.to) + Duration::seconds(1)
        } else {
            self.to
        };
        (self.from.format("%Y-%m-%d %H:%M:%S").to_string(), to.format("%Y-%m-%d %H:%M:%S").to_string())
    }

    /// Description of the window for tool responses.
    pub fn describe(&self, field: &str, from_expr: &str, to_expr: &str) -> Value {
        json!({
            "field": field,
            "from": self.from_iso(),
            "to": self.to_iso(),
            "from_expression": from_expr,
            "to_expression": to_expr,
            "timezone": "UTC",
        })
    }
}

pub fn iso_millis(time: DateTime<Utc>) -> String {
    time.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

fn time_syntax_help(expression: &str) -> String {
    format!(
        "Invalid time \"{expression}\". Use now, now-15m, now-1h, now-24h, now-7d (general form now±N with unit \
         s, m, h, d, w, M or y, optionally rounded with /unit such as now-1d/d), an absolute ISO-8601 timestamp \
         such as 2026-09-26T10:00:00Z (no zone means UTC), a date such as 2026-09-26, or epoch seconds/milliseconds."
    )
}

fn add_units(time: DateTime<Utc>, amount: i64, unit: char) -> Option<DateTime<Utc>> {
    let months = |count: i64| -> Option<DateTime<Utc>> {
        let magnitude = Months::new(u32::try_from(count.unsigned_abs()).ok()?);
        if count >= 0 {
            time.checked_add_months(magnitude)
        } else {
            time.checked_sub_months(magnitude)
        }
    };
    match unit {
        's' => time.checked_add_signed(Duration::try_seconds(amount)?),
        'm' => time.checked_add_signed(Duration::try_minutes(amount)?),
        'h' => time.checked_add_signed(Duration::try_hours(amount)?),
        'd' => time.checked_add_signed(Duration::try_days(amount)?),
        'w' => time.checked_add_signed(Duration::try_weeks(amount)?),
        'M' => months(amount),
        'y' => months(amount.checked_mul(12)?),
        _ => None,
    }
}

fn round_time(time: DateTime<Utc>, unit: char, round_up: bool) -> Option<DateTime<Utc>> {
    let date = time.date_naive();
    let start = match unit {
        'y' => NaiveDate::from_ymd_opt(date.year(), 1, 1)?.and_hms_opt(0, 0, 0)?,
        'M' => NaiveDate::from_ymd_opt(date.year(), date.month(), 1)?.and_hms_opt(0, 0, 0)?,
        'w' => (date - Duration::days(date.weekday().num_days_from_monday() as i64)).and_hms_opt(0, 0, 0)?,
        'd' => date.and_hms_opt(0, 0, 0)?,
        'h' => date.and_hms_opt(time.hour(), 0, 0)?,
        'm' => date.and_hms_opt(time.hour(), time.minute(), 0)?,
        's' => date.and_hms_opt(time.hour(), time.minute(), time.second())?,
        _ => return None,
    };
    let start = Utc.from_utc_datetime(&start);
    if round_up {
        Some(add_units(start, 1, unit)? - Duration::milliseconds(1))
    } else {
        Some(start)
    }
}

/// Parse a date-math expression (`now`, `now-15m`, `now-1d/d`) or an absolute
/// time. `round_up` rounds `/unit` (and a bare date) to the end of the unit,
/// as Elasticsearch does for range ends. All arithmetic is in UTC.
pub fn parse_date_math(expression: &str, now: DateTime<Utc>, round_up: bool) -> Result<DateTime<Utc>, String> {
    let text = expression.trim();
    if text.is_empty() {
        return Err(time_syntax_help(expression));
    }
    if let Some(mut rest) = text.strip_prefix("now") {
        let mut time = now;
        while !rest.is_empty() {
            let mut chars = rest.chars();
            let op = chars.next().unwrap_or_default();
            if op == '/' {
                let unit = chars.next().ok_or_else(|| time_syntax_help(expression))?;
                time = round_time(time, unit, round_up).ok_or_else(|| time_syntax_help(expression))?;
                rest = &rest[1 + unit.len_utf8()..];
                continue;
            }
            if op != '+' && op != '-' {
                return Err(time_syntax_help(expression));
            }
            let digits: String = rest[1..].chars().take_while(char::is_ascii_digit).collect();
            let unit = rest[1 + digits.len()..].chars().next().ok_or_else(|| time_syntax_help(expression))?;
            let amount: i64 =
                if digits.is_empty() { 1 } else { digits.parse().map_err(|_| time_syntax_help(expression))? };
            let amount = if op == '-' { -amount } else { amount };
            time = add_units(time, amount, unit).ok_or_else(|| time_syntax_help(expression))?;
            rest = &rest[1 + digits.len() + unit.len_utf8()..];
        }
        return Ok(time);
    }
    if text.chars().all(|ch| ch.is_ascii_digit()) && text.len() >= 9 {
        let number: i64 = text.parse().map_err(|_| time_syntax_help(expression))?;
        // Ten digits or fewer are epoch seconds (valid until 2286), longer are milliseconds.
        let millis = if text.len() <= 10 { number.checked_mul(1000) } else { Some(number) };
        return millis
            .and_then(|millis| Utc.timestamp_millis_opt(millis).single())
            .ok_or_else(|| time_syntax_help(expression));
    }
    let normalized = if text.len() > 10 && text.as_bytes()[10] == b' ' {
        format!("{}T{}", &text[..10], &text[11..])
    } else {
        text.to_string()
    };
    if let Ok(time) = DateTime::parse_from_rfc3339(&normalized) {
        return Ok(time.with_timezone(&Utc));
    }
    for format in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M"] {
        if let Ok(time) = NaiveDateTime::parse_from_str(&normalized, format) {
            return Ok(Utc.from_utc_datetime(&time));
        }
    }
    for format in ["%Y-%m-%dT%H:%M:%S%.f%#z", "%Y-%m-%dT%H:%M%#z"] {
        if let Ok(time) = DateTime::parse_from_str(&normalized, format) {
            return Ok(time.with_timezone(&Utc));
        }
    }
    if let Ok(date) = NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        let start = Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0).unwrap_or_default());
        return if round_up {
            round_time(start, 'd', true).ok_or_else(|| time_syntax_help(expression))
        } else {
            Ok(start)
        };
    }
    Err(time_syntax_help(expression))
}

/// Resolve `from` / `to` (defaults: the last hour) into an absolute window.
pub fn resolve_time_window(from: Option<&str>, to: Option<&str>, now: DateTime<Utc>) -> Result<TimeWindow, String> {
    let from_expr = from.map(str::trim).filter(|value| !value.is_empty()).unwrap_or(DEFAULT_WINDOW_FROM);
    let to_expr = to.map(str::trim).filter(|value| !value.is_empty()).unwrap_or(DEFAULT_WINDOW_TO);
    let from = parse_date_math(from_expr, now, false)?;
    let to = parse_date_math(to_expr, now, true)?;
    if from > to {
        return Err(format!(
            "The time window is empty: from \"{from_expr}\" ({}) is after to \"{to_expr}\" ({}).",
            iso_millis(from),
            iso_millis(to)
        ));
    }
    Ok(TimeWindow { from, to })
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

const INDEX_PATTERN_KEEP: &AsciiSet =
    &NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'.').remove(b'*').remove(b',');

/// Validate an index pattern and encode it for a URL path segment, keeping
/// `*` and `,` (multi-target) readable. Cross-cluster `cluster:index` targets
/// keep their colon.
pub fn encode_index_pattern(pattern: &str) -> Result<String, String> {
    let parts: Vec<&str> = pattern.split(',').map(str::trim).filter(|part| !part.is_empty()).collect();
    if parts.is_empty() {
        return Err("An index or index pattern is required, e.g. logs-*.".to_string());
    }
    let mut encoded = Vec::with_capacity(parts.len());
    for part in parts {
        if part.chars().any(|ch| ch.is_whitespace() || ch.is_control()) {
            return Err(format!("Invalid index pattern \"{part}\": whitespace is not allowed."));
        }
        encoded.push(
            part.split(':')
                .map(|piece| utf8_percent_encode(piece, INDEX_PATTERN_KEEP).to_string())
                .collect::<Vec<_>>()
                .join(":"),
        );
    }
    Ok(encoded.join(","))
}

// ---------------------------------------------------------------------------
// _search
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryLanguage {
    Dql,
    Lucene,
}

impl QueryLanguage {
    pub fn parse(value: Option<&str>) -> Result<Self, String> {
        match value.map(|value| value.trim().to_ascii_lowercase()).as_deref() {
            None | Some("") | Some("dql") | Some("kql") => Ok(Self::Dql),
            Some("lucene") => Ok(Self::Lucene),
            Some(other) => Err(format!("Unsupported query language \"{other}\". Use dql or lucene.")),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dql => "dql",
            Self::Lucene => "lucene",
        }
    }
}

/// Wildcard field names (other than `*`) used by a DQL query; these need the
/// index's field list to expand.
pub fn dql_wildcard_fields(query: &str) -> Vec<String> {
    parse_dql(query)
        .map(|node| {
            node.field_names()
                .into_iter()
                .filter(|name| name.contains('*') && *name != "*")
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Main query clause for DQL / Lucene. An empty query matches everything.
pub fn build_query_clause(
    language: QueryLanguage,
    query: &str,
    fields: &[DqlKnownField],
) -> Result<Value, DqlSyntaxError> {
    let text = query.trim();
    if text.is_empty() {
        return Ok(json!({ "match_all": {} }));
    }
    match language {
        QueryLanguage::Lucene => Ok(json!({ "query_string": { "query": text, "analyze_wildcard": true } })),
        QueryLanguage::Dql => dql_to_dsl(text, fields),
    }
}

pub fn time_range_clause(time_field: &str, window: &TimeWindow) -> Value {
    json!({
        "range": {
            time_field: { "gte": window.from_iso(), "lte": window.to_iso(), "format": "strict_date_optional_time" }
        }
    })
}

/// `_search` body: the query clause, the optional time filter, newest first
/// on `sort_field`, and exact totals.
pub fn build_search_body(
    query_clause: Value,
    time_filter: Option<(&str, &TimeWindow)>,
    sort_field: Option<&str>,
    size: usize,
) -> Value {
    let mut bool_query = Map::new();
    if query_clause.get("match_all").is_none() {
        bool_query.insert("must".into(), json!([query_clause]));
    }
    if let Some((field, window)) = time_filter {
        bool_query.insert("filter".into(), json!([time_range_clause(field, window)]));
    }
    let query = if bool_query.is_empty() { json!({ "match_all": {} }) } else { json!({ "bool": bool_query }) };
    let mut body = json!({ "size": size, "track_total_hits": true, "query": query });
    if let Some(field) = sort_field {
        body["sort"] = json!([{ field: { "order": "desc", "unmapped_type": "boolean" } }]);
    }
    body
}

/// `hits.total` as `(value, relation)`; `relation` is `eq` or `gte`.
pub fn read_total_hits(response: &Value) -> (u64, &'static str) {
    match response.pointer("/hits/total") {
        Some(Value::Number(number)) => (number.as_u64().unwrap_or(0), "eq"),
        Some(Value::Object(total)) => (
            total.get("value").and_then(Value::as_u64).unwrap_or(0),
            if total.get("relation").and_then(Value::as_str) == Some("gte") { "gte" } else { "eq" },
        ),
        _ => (0, "eq"),
    }
}

/// Value at a dotted path, following nested objects and also flattened
/// dotted keys (`{"kubernetes.pod": ...}`).
pub fn lookup_path<'a>(source: &'a Value, path: &str) -> Option<&'a Value> {
    let object = source.as_object()?;
    if let Some(value) = object.get(path) {
        return Some(value);
    }
    let mut split = path.char_indices().filter(|(_, ch)| *ch == '.').map(|(index, _)| index);
    split.find_map(|index| object.get(&path[..index]).and_then(|child| lookup_path(child, &path[index + 1..])))
}

/// Copy of `value` with long strings cut to `max_chars` characters.
pub fn truncate_long_strings(value: &Value, max_chars: usize) -> Value {
    match value {
        Value::String(text) => {
            let count = text.chars().count();
            if count <= max_chars {
                value.clone()
            } else {
                let kept: String = text.chars().take(max_chars).collect();
                Value::String(format!("{kept}… [truncated {} more chars]", count - max_chars))
            }
        }
        Value::Array(items) => Value::Array(items.iter().map(|item| truncate_long_strings(item, max_chars)).collect()),
        Value::Object(map) => {
            Value::Object(map.iter().map(|(key, item)| (key.clone(), truncate_long_strings(item, max_chars))).collect())
        }
        _ => value.clone(),
    }
}

/// Compact hits: `_index`, `_id`, the time value and the (truncated) `_source`.
pub fn compact_hits(response: &Value, time_field: Option<&str>) -> Vec<Value> {
    let Some(hits) = response.pointer("/hits/hits").and_then(Value::as_array) else {
        return Vec::new();
    };
    hits.iter()
        .map(|hit| {
            let source = hit.get("_source").cloned().unwrap_or(Value::Null);
            let mut compact = Map::new();
            compact.insert("_index".into(), hit.get("_index").cloned().unwrap_or(Value::Null));
            compact.insert("_id".into(), hit.get("_id").cloned().unwrap_or(Value::Null));
            if let Some(field) = time_field {
                let time = lookup_path(&source, field)
                    .or_else(|| hit.get("fields").and_then(|fields| fields.get(field)))
                    .or_else(|| hit.get("sort").and_then(|sort| sort.get(0)))
                    .cloned()
                    .unwrap_or(Value::Null);
                compact.insert("time".into(), time);
            }
            compact.insert("_source".into(), truncate_long_strings(&source, MAX_VALUE_CHARS));
            Value::Object(compact)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Dashboards saved index patterns
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedIndexPattern {
    pub id: String,
    pub title: String,
    pub time_field: Option<String>,
    /// Saved objects index the pattern came from (tenant indices differ from `.kibana`).
    pub saved_in: String,
}

impl SavedIndexPattern {
    pub fn to_json(&self) -> Value {
        json!({ "title": self.title, "time_field": self.time_field, "id": self.id, "saved_in": self.saved_in })
    }
}

/// `_search` body listing `index-pattern` saved objects.
pub fn saved_index_patterns_request_body() -> Value {
    json!({
        "size": 1000,
        "query": { "term": { "type": "index-pattern" } },
        "_source": ["type", "index-pattern.title", "index-pattern.timeFieldName"]
    })
}

pub fn saved_index_patterns_path() -> String {
    format!(
        "/{DASHBOARDS_SAVED_OBJECTS_PATTERN}/_search?ignore_unavailable=true&allow_no_indices=true&expand_wildcards=open,hidden"
    )
}

/// Parse saved `index-pattern` objects, de-duplicated (`.kibana` is usually
/// an alias of `.kibana_N`, which the wildcard also matches) and sorted by title.
pub fn parse_saved_index_patterns(response: &Value) -> Vec<SavedIndexPattern> {
    let mut patterns: Vec<SavedIndexPattern> = Vec::new();
    for hit in response.pointer("/hits/hits").and_then(Value::as_array).into_iter().flatten() {
        let Some(saved) = hit.pointer("/_source/index-pattern") else { continue };
        let Some(title) = saved.get("title").and_then(Value::as_str).filter(|title| !title.trim().is_empty()) else {
            continue;
        };
        let time_field =
            saved.get("timeFieldName").and_then(Value::as_str).filter(|field| !field.is_empty()).map(str::to_string);
        let id = hit.get("_id").and_then(Value::as_str).unwrap_or_default();
        let id = id.strip_prefix("index-pattern:").unwrap_or(id).to_string();
        let saved_in = hit.get("_index").and_then(Value::as_str).unwrap_or_default().to_string();
        if patterns.iter().any(|existing| existing.title == title && existing.time_field == time_field) {
            continue;
        }
        patterns.push(SavedIndexPattern { id, title: title.to_string(), time_field, saved_in });
    }
    patterns.sort_by(|a, b| a.title.cmp(&b.title));
    patterns
}

// ---------------------------------------------------------------------------
// _cat/indices and _resolve/index
// ---------------------------------------------------------------------------

/// Wildcard base of an index name: trailing date/number parts become `*`
/// (`logs-local-2026.09.20` -> `logs-local-*`). `None` without such a suffix.
pub fn wildcard_base(name: &str) -> Option<String> {
    let trimmed = name.trim_end_matches('*');
    let bytes = trimmed.as_bytes();
    let is_sep = |byte: u8| matches!(byte, b'.' | b'_' | b'-');
    // Find the start of the trailing run of digits and separators that begins with a digit.
    let mut start = bytes.len();
    while start > 0 && (bytes[start - 1].is_ascii_digit() || is_sep(bytes[start - 1])) {
        start -= 1;
    }
    while start < bytes.len() && !bytes[start].is_ascii_digit() {
        start += 1;
    }
    // The prefix must end with a separator preceded by a non-digit, non-separator character.
    if start >= bytes.len() || start < 2 || !is_sep(bytes[start - 1]) {
        return None;
    }
    let before = bytes[start - 2];
    if before.is_ascii_digit() || is_sep(before) {
        return None;
    }
    Some(format!("{}*", &trimmed[..start]))
}

/// Concrete indices from `_cat/indices?format=json`, hidden (dot) indices
/// counted separately, plus derived rollover patterns (most indices first).
pub fn summarize_cat_indices(response: &Value) -> (Vec<Value>, usize, Vec<Value>) {
    let mut indices = Vec::new();
    let mut hidden = 0;
    let mut patterns: std::collections::BTreeMap<String, usize> = Default::default();
    for row in response.as_array().into_iter().flatten() {
        let Some(name) = row.get("index").and_then(Value::as_str) else { continue };
        if name.starts_with('.') {
            hidden += 1;
            continue;
        }
        if let Some(base) = wildcard_base(name) {
            *patterns.entry(base).or_default() += 1;
        }
        let mut entry = Map::new();
        entry.insert("index".into(), json!(name));
        for (key, output) in
            [("health", "health"), ("status", "status"), ("docs.count", "docs"), ("store.size", "size")]
        {
            if let Some(value) = row.get(key).filter(|value| !value.is_null()) {
                let value = match value.as_str().and_then(|text| text.parse::<u64>().ok()) {
                    Some(number) if key == "docs.count" => json!(number),
                    _ => value.clone(),
                };
                entry.insert(output.into(), value);
            }
        }
        indices.push(Value::Object(entry));
    }
    indices.sort_by(|a, b| a["index"].as_str().cmp(&b["index"].as_str()));
    let mut derived: Vec<(String, usize)> = patterns.into_iter().filter(|(_, count)| *count > 1).collect();
    derived.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let derived = derived.into_iter().map(|(pattern, count)| json!({ "pattern": pattern, "indices": count })).collect();
    (indices, hidden, derived)
}

/// Compact `_resolve/index/<pattern>` response.
pub fn summarize_resolve_index(response: &Value) -> Value {
    let names = |key: &str| -> Vec<Value> {
        response
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|entry| {
                let name = entry.get("name")?.as_str()?;
                let mut compact = Map::new();
                compact.insert("name".into(), json!(name));
                for field in ["aliases", "attributes", "indices", "backing_indices", "data_stream", "timestamp_field"] {
                    if let Some(value) = entry.get(field) {
                        compact.insert(field.into(), value.clone());
                    }
                }
                Some(Value::Object(compact))
            })
            .collect()
    };
    json!({ "indices": names("indices"), "aliases": names("aliases"), "data_streams": names("data_streams") })
}

// ---------------------------------------------------------------------------
// _field_caps
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldCapability {
    pub name: String,
    /// The mapped type, or `conflict` when indices disagree (see `types`).
    pub field_type: String,
    pub types: Vec<String>,
    pub searchable: bool,
    pub aggregatable: bool,
}

impl FieldCapability {
    pub fn to_json(&self) -> Value {
        let mut value = json!({
            "name": self.name,
            "type": self.field_type,
            "searchable": self.searchable,
            "aggregatable": self.aggregatable,
        });
        if self.types.len() > 1 {
            value["types"] = json!(self.types);
        }
        value
    }

    pub fn is_date(&self) -> bool {
        self.types.iter().all(|kind| kind == "date" || kind == "date_nanos") && !self.types.is_empty()
    }
}

pub fn field_caps_path(encoded_pattern: &str, fields: &str) -> String {
    format!("/{encoded_pattern}/_field_caps?fields={fields}&ignore_unavailable=true&allow_no_indices=true")
}

/// Leaf fields from a `_field_caps` response, sorted by name. Object/nested
/// containers and internal metadata fields (other than `_id`/`_index`) are skipped.
pub fn parse_field_caps(response: &Value) -> Vec<FieldCapability> {
    let Some(fields) = response.get("fields").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (name, per_type) in fields {
        if name.starts_with('_') && name != "_id" && name != "_index" {
            continue;
        }
        let Some(per_type) = per_type.as_object() else { continue };
        let mut types: Vec<String> = per_type
            .keys()
            .filter(|kind| !matches!(kind.as_str(), "object" | "nested" | "unmapped"))
            .cloned()
            .collect();
        types.sort();
        if types.is_empty() {
            continue;
        }
        let all = |flag: &str, partial: &str| {
            types.iter().all(|kind| {
                let entry = &per_type[kind];
                entry.get(flag).and_then(Value::as_bool).unwrap_or(false) && entry.get(partial).is_none()
            })
        };
        let searchable = all("searchable", "non_searchable_indices");
        let aggregatable = all("aggregatable", "non_aggregatable_indices");
        let field_type = if types.len() == 1 { types[0].clone() } else { "conflict".to_string() };
        out.push(FieldCapability { name: name.clone(), field_type, types, searchable, aggregatable });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

pub fn known_dql_fields(fields: &[FieldCapability]) -> Vec<DqlKnownField> {
    fields.iter().map(|field| DqlKnownField { name: field.name.clone(), searchable: field.searchable }).collect()
}

// ---------------------------------------------------------------------------
// SQL / PPL
// ---------------------------------------------------------------------------

/// Accept exactly one `SELECT` statement; returns it without a trailing `;`.
pub fn validate_select_sql(query: &str) -> Result<String, String> {
    let body = strip_leading_sql_comments(query).trim();
    let body = body.trim_end_matches(|ch: char| ch == ';' || ch.is_whitespace());
    let leading: String = body
        .trim_start_matches('(')
        .chars()
        .take_while(char::is_ascii_alphabetic)
        .collect::<String>()
        .to_ascii_lowercase();
    if leading != "select" {
        return Err(format!(
            "Only SELECT statements are allowed; this is a read-only tool (got \"{}\"). Use dbx_opensearch_list_indices \
             / dbx_opensearch_fields for metadata.",
            if leading.is_empty() { body.chars().take(20).collect::<String>() } else { leading.to_ascii_uppercase() }
        ));
    }
    if top_level_positions(body, ';').next().is_some() {
        return Err("Only a single SELECT statement is allowed per call.".to_string());
    }
    Ok(body.to_string())
}

/// Byte offsets of `needle` outside quotes (`'`, `"`, `` ` ``).
fn top_level_positions(text: &str, needle: char) -> impl Iterator<Item = usize> + '_ {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    text.char_indices().filter_map(move |(index, ch)| {
        if escaped {
            escaped = false;
            return None;
        }
        match quote {
            Some(_) if ch == '\\' => {
                escaped = true;
                None
            }
            Some(open) if ch == open => {
                quote = None;
                None
            }
            Some(_) => None,
            None if matches!(ch, '\'' | '"' | '`') => {
                quote = Some(ch);
                None
            }
            None if ch == needle => Some(index),
            None => None,
        }
    })
}

fn quote_ppl_field(field: &str) -> String {
    let mut chars = field.chars();
    let plain = chars.next().is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '.');
    if plain {
        field.to_string()
    } else {
        format!("`{}`", field.replace('`', ""))
    }
}

/// Validate a read-only PPL query that starts with `source=` and insert the
/// time window as a `where` right after the source command (OpenSearch 1.x
/// PPL has no `NOW()`). With `window == None` only validation happens.
pub fn insert_ppl_time_window(query: &str, time_field: &str, window: Option<&TimeWindow>) -> Result<String, String> {
    let trimmed = query.trim().trim_start_matches('|').trim_start();
    let lower = trimmed.to_ascii_lowercase();
    let after_search = lower.strip_prefix("search").map(str::trim_start).unwrap_or(&lower);
    let starts_with_source = after_search.strip_prefix("source").is_some_and(|rest| rest.trim_start().starts_with('='));
    if !starts_with_source {
        return Err(
            "PPL must start with source=<index or pattern>, e.g. source=logs-* | stats count() by level.".to_string()
        );
    }
    if !ppl_query_is_read_only(trimmed) {
        return Err("Only read-only PPL is allowed (no ml / kmeans / ad commands).".to_string());
    }
    let Some(window) = window else {
        return Ok(trimmed.to_string());
    };
    let field = quote_ppl_field(time_field);
    let (from, to) = window.ppl_bounds();
    let clause = format!("where {field} >= '{from}' and {field} <= '{to}'");
    Ok(match top_level_positions(trimmed, '|').next() {
        Some(index) => {
            format!("{} | {clause} | {}", trimmed[..index].trim_end(), trimmed[index + 1..].trim_start())
        }
        None => format!("{trimmed} | {clause}"),
    })
}

/// Index pattern named by a PPL `source=` command, if any.
pub fn ppl_source_pattern(query: &str) -> Option<String> {
    let trimmed = query.trim().trim_start_matches('|').trim_start();
    let lower = trimmed.to_ascii_lowercase();
    let start = lower.find("source")?;
    let rest = trimmed[start + "source".len()..].trim_start().strip_prefix('=')?.trim_start();
    let pattern: String = rest.chars().take_while(|ch| !ch.is_whitespace() && *ch != '|').collect();
    let pattern = pattern.trim_matches('`').to_string();
    (!pattern.is_empty()).then_some(pattern)
}

/// SQL/PPL response as `{columns, rows, total, returned}`: OpenSearch JDBC
/// (`schema` + `datarows`) or Elasticsearch SQL JSON (`columns` + `rows`).
pub fn shape_tabular_response(response: &Value, limit: usize) -> Result<Value, String> {
    let (columns, rows, total) = if let Some(schema) = response.get("schema").and_then(Value::as_array) {
        (schema, response.get("datarows"), response.get("total").and_then(Value::as_u64))
    } else if let Some(columns) = response.get("columns").and_then(Value::as_array) {
        (columns, response.get("rows"), None)
    } else {
        return Err("Unexpected SQL/PPL response: no schema/columns.".to_string());
    };
    let columns: Vec<Value> = columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let name = column
                .get("alias")
                .and_then(Value::as_str)
                .filter(|alias| !alias.is_empty())
                .or_else(|| column.get("name").and_then(Value::as_str))
                .map(str::to_string)
                .unwrap_or_else(|| format!("column_{}", index + 1));
            json!({ "name": name, "type": column.get("type").cloned().unwrap_or(Value::Null) })
        })
        .collect();
    let rows = rows.and_then(Value::as_array).cloned().unwrap_or_default();
    let fetched = rows.len();
    let rows: Vec<Value> = rows.iter().take(limit).map(|row| truncate_long_strings(row, MAX_VALUE_CHARS)).collect();
    let mut shaped = json!({
        "columns": columns,
        "rows": rows,
        "returned": rows.len(),
        "fetched": fetched,
    });
    if let Some(total) = total {
        shaped["total"] = json!(total);
    }
    if fetched > rows.len() {
        shaped["notes"] = json!([format!(
            "Showing the first {} of {fetched} fetched rows (limit). Add LIMIT / aggregate in the query to narrow it.",
            rows.len()
        )]);
    }
    Ok(shaped)
}

/// Readable error from a non-2xx search/SQL/PPL response body.
pub fn error_reason(status: u16, body: &str) -> String {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let reason = parsed.as_ref().and_then(|body| {
        let error = body.get("error")?;
        if let Some(text) = error.as_str() {
            return Some(text.to_string());
        }
        let root = error.pointer("/root_cause/0/reason").and_then(Value::as_str);
        let reason = error.get("reason").and_then(Value::as_str);
        let details = error.get("details").and_then(Value::as_str);
        let caused = error.pointer("/caused_by/reason").and_then(Value::as_str);
        let failed = error.pointer("/failed_shards/0/reason/reason").and_then(Value::as_str);
        let parts: Vec<&str> = [reason, root, caused, failed, details]
            .into_iter()
            .flatten()
            .filter(|part| !part.is_empty())
            .fold(Vec::new(), |mut acc, part| {
                if !acc.contains(&part) {
                    acc.push(part);
                }
                acc
            });
        (!parts.is_empty()).then(|| parts.join(" | "))
    });
    let reason = reason.unwrap_or_else(|| body.chars().take(2_000).collect());
    format!("HTTP {status}: {reason}")
}

// ---------------------------------------------------------------------------
// Response size cap
// ---------------------------------------------------------------------------

/// Serialize `value`, dropping trailing items of the top-level `array_key`
/// array until the JSON fits in `max_bytes`, with a note saying so.
pub fn fit_response(mut value: Value, array_key: &str, max_bytes: usize) -> String {
    let mut text = value.to_string();
    if text.len() <= max_bytes {
        return text;
    }
    let total = value.get(array_key).and_then(Value::as_array).map_or(0, Vec::len);
    let mut keep = total;
    while text.len() > max_bytes && keep > 0 {
        // Shrink proportionally, then step down one at a time near the limit.
        let ratio = max_bytes as f64 / text.len() as f64;
        keep = ((keep as f64 * ratio) as usize).min(keep - 1);
        if let Some(items) = value.get_mut(array_key).and_then(Value::as_array_mut) {
            items.truncate(keep);
        }
        value["response_truncated"] = json!(format!(
            "Response capped at {} KB: showing {keep} of {total} {array_key}. Narrow the query, lower limit, or \
             select fewer fields.",
            max_bytes / 1024
        ));
        text = value.to_string();
    }
    if text.len() > max_bytes {
        let mut end = max_bytes;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        return format!("{}… [response truncated at {} KB]", &text[..end], max_bytes / 1024);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 26, 10, 30, 15).unwrap() + Duration::milliseconds(250)
    }

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn parses_relative_date_math() {
        assert_eq!(parse_date_math("now", now(), false).unwrap(), now());
        assert_eq!(parse_date_math("now-15m", now(), false).unwrap(), now() - Duration::minutes(15));
        assert_eq!(parse_date_math("now-1h", now(), false).unwrap(), now() - Duration::hours(1));
        assert_eq!(parse_date_math("now-24h", now(), false).unwrap(), now() - Duration::hours(24));
        assert_eq!(parse_date_math("now-7d", now(), false).unwrap(), now() - Duration::days(7));
        assert_eq!(parse_date_math("now+30s", now(), false).unwrap(), now() + Duration::seconds(30));
        assert_eq!(parse_date_math("now-2w", now(), false).unwrap(), now() - Duration::weeks(2));
        assert_eq!(parse_date_math("now-1M", now(), false).unwrap(), at("2026-08-26T10:30:15.250Z"));
        assert_eq!(parse_date_math("now-1y", now(), false).unwrap(), at("2025-09-26T10:30:15.250Z"));
        assert_eq!(parse_date_math("now-1d-2h", now(), false).unwrap(), at("2026-09-25T08:30:15.250Z"));
    }

    #[test]
    fn rounds_date_math_down_and_up() {
        assert_eq!(parse_date_math("now/d", now(), false).unwrap(), at("2026-09-26T00:00:00Z"));
        assert_eq!(parse_date_math("now/d", now(), true).unwrap(), at("2026-09-26T23:59:59.999Z"));
        assert_eq!(parse_date_math("now-1d/d", now(), false).unwrap(), at("2026-09-25T00:00:00Z"));
        // 2026-09-26 is a Saturday; weeks start on Monday.
        assert_eq!(parse_date_math("now/w", now(), false).unwrap(), at("2026-09-21T00:00:00Z"));
        assert_eq!(parse_date_math("now/M", now(), false).unwrap(), at("2026-09-01T00:00:00Z"));
        assert_eq!(parse_date_math("now/y", now(), false).unwrap(), at("2026-01-01T00:00:00Z"));
        assert_eq!(parse_date_math("now/h", now(), true).unwrap(), at("2026-09-26T10:59:59.999Z"));
    }

    #[test]
    fn parses_absolute_times_as_utc() {
        assert_eq!(parse_date_math("2026-09-26T08:00:00Z", now(), false).unwrap(), at("2026-09-26T08:00:00Z"));
        assert_eq!(parse_date_math("2026-09-26T08:00:00+02:00", now(), false).unwrap(), at("2026-09-26T06:00:00Z"));
        assert_eq!(parse_date_math("2026-09-26T08:00:00.5", now(), false).unwrap(), at("2026-09-26T08:00:00.5Z"));
        assert_eq!(parse_date_math("2026-09-26 08:00:00", now(), false).unwrap(), at("2026-09-26T08:00:00Z"));
        assert_eq!(parse_date_math("2026-09-26T08:00", now(), false).unwrap(), at("2026-09-26T08:00:00Z"));
        assert_eq!(parse_date_math("2026-09-26", now(), false).unwrap(), at("2026-09-26T00:00:00Z"));
        assert_eq!(parse_date_math("2026-09-26", now(), true).unwrap(), at("2026-09-26T23:59:59.999Z"));
        assert_eq!(parse_date_math("1790000000", now(), false).unwrap(), at("2026-09-21T14:13:20Z"));
        assert_eq!(parse_date_math("1790000000123", now(), false).unwrap(), at("2026-09-21T14:13:20.123Z"));
    }

    #[test]
    fn rejects_bad_time_expressions_with_help() {
        for bad in ["", "yesterday", "now-", "now-5x", "now*2", "2026-13-40", "now/q"] {
            let error = parse_date_math(bad, now(), false).unwrap_err();
            assert!(error.contains("now-15m"), "{bad}: {error}");
        }
    }

    #[test]
    fn resolves_windows_with_last_hour_default() {
        let window = resolve_time_window(None, None, now()).unwrap();
        assert_eq!(window.from_iso(), "2026-09-26T09:30:15.250Z");
        assert_eq!(window.to_iso(), "2026-09-26T10:30:15.250Z");
        let window = resolve_time_window(Some("now-7d"), Some(" "), now()).unwrap();
        assert_eq!(window.from, now() - Duration::days(7));
        assert_eq!(window.to, now());
        let error = resolve_time_window(Some("now"), Some("now-1h"), now()).unwrap_err();
        assert!(error.contains("empty"), "{error}");
        let described = window.describe("@timestamp", "now-7d", "now");
        assert_eq!(described["from"], "2026-09-19T10:30:15.250Z");
        assert_eq!(described["timezone"], "UTC");
    }

    #[test]
    fn ppl_bounds_round_the_end_up_to_whole_seconds() {
        let window = resolve_time_window(None, None, now()).unwrap();
        assert_eq!(window.ppl_bounds(), ("2026-09-26 09:30:15".to_string(), "2026-09-26 10:30:16".to_string()));
    }

    #[test]
    fn inserts_ppl_window_right_after_source() {
        let window = TimeWindow { from: at("2026-09-26T09:00:00Z"), to: at("2026-09-26T10:00:00Z") };
        assert_eq!(
            insert_ppl_time_window("source=logs-* | stats count() by level", "@timestamp", Some(&window)).unwrap(),
            "source=logs-* | where `@timestamp` >= '2026-09-26 09:00:00' and `@timestamp` <= '2026-09-26 10:00:00' \
             | stats count() by level"
        );
        assert_eq!(
            insert_ppl_time_window("  source = logs-local-*,app-*", "ts", Some(&window)).unwrap(),
            "source = logs-local-*,app-* | where ts >= '2026-09-26 09:00:00' and ts <= '2026-09-26 10:00:00'"
        );
        assert_eq!(
            insert_ppl_time_window(
                "search source=logs level='a|b' | where x = 1 | head 5",
                "event.created",
                Some(&window)
            )
            .unwrap(),
            "search source=logs level='a|b' | where event.created >= '2026-09-26 09:00:00' and event.created <= \
             '2026-09-26 10:00:00' | where x = 1 | head 5"
        );
        assert_eq!(insert_ppl_time_window("source=logs | head 5", "@timestamp", None).unwrap(), "source=logs | head 5");
    }

    #[test]
    fn reads_the_ppl_source_pattern() {
        assert_eq!(ppl_source_pattern("source=logs-* | head 5").as_deref(), Some("logs-*"));
        assert_eq!(ppl_source_pattern("search source = `app-*`").as_deref(), Some("app-*"));
        assert_eq!(ppl_source_pattern("describe logs"), None);
    }

    #[test]
    fn rejects_ppl_without_source_or_with_ml() {
        assert!(insert_ppl_time_window("stats count()", "@timestamp", None).unwrap_err().contains("source="));
        assert!(insert_ppl_time_window("describe logs", "@timestamp", None).unwrap_err().contains("source="));
        assert!(insert_ppl_time_window("source=logs | kmeans centroids=3", "@timestamp", None)
            .unwrap_err()
            .contains("read-only"));
    }

    #[test]
    fn validates_single_select_statements() {
        assert_eq!(
            validate_select_sql("  SELECT * FROM `logs-*` LIMIT 5; ").unwrap(),
            "SELECT * FROM `logs-*` LIMIT 5"
        );
        assert_eq!(validate_select_sql("-- note\nselect 1").unwrap(), "select 1");
        assert!(validate_select_sql("SELECT ';' FROM logs").is_ok());
        assert!(validate_select_sql("DELETE FROM logs").unwrap_err().contains("Only SELECT"));
        assert!(validate_select_sql("SHOW TABLES LIKE %").unwrap_err().contains("SHOW"));
        assert!(validate_select_sql("SELECT 1; DELETE FROM logs").unwrap_err().contains("single"));
        assert!(validate_select_sql("").unwrap_err().contains("Only SELECT"));
    }

    #[test]
    fn encodes_index_patterns() {
        assert_eq!(encode_index_pattern(" logs-*, app_1.x ").unwrap(), "logs-*,app_1.x");
        assert_eq!(encode_index_pattern("remote:logs-*").unwrap(), "remote:logs-*");
        assert_eq!(encode_index_pattern("<logs-{now/d}>").unwrap(), "%3Clogs-%7Bnow%2Fd%7D%3E");
        assert_eq!(encode_index_pattern("a/../b").unwrap(), "a%2F..%2Fb");
        assert!(encode_index_pattern(" , ").is_err());
        assert!(encode_index_pattern("logs *").is_err());
    }

    #[test]
    fn builds_query_clauses_like_discover() {
        assert_eq!(
            build_query_clause(QueryLanguage::Lucene, "level:ERROR AND msg*", &[]).unwrap(),
            json!({ "query_string": { "query": "level:ERROR AND msg*", "analyze_wildcard": true } })
        );
        assert_eq!(
            build_query_clause(QueryLanguage::Dql, "level:ERROR", &[]).unwrap(),
            json!({ "match": { "level": "ERROR" } })
        );
        assert_eq!(build_query_clause(QueryLanguage::Dql, "  ", &[]).unwrap(), json!({ "match_all": {} }));
        assert!(build_query_clause(QueryLanguage::Dql, "level:(", &[]).is_err());
        assert_eq!(QueryLanguage::parse(None).unwrap(), QueryLanguage::Dql);
        assert_eq!(QueryLanguage::parse(Some("Lucene")).unwrap(), QueryLanguage::Lucene);
        assert!(QueryLanguage::parse(Some("sql")).is_err());
        assert_eq!(dql_wildcard_fields("kubernetes.*:web and *:* and level:x"), vec!["kubernetes.*".to_string()]);
    }

    #[test]
    fn builds_search_bodies_with_time_filter_and_sort() {
        let window = TimeWindow { from: at("2026-09-24T00:00:00Z"), to: at("2026-09-25T00:00:00Z") };
        let body = build_search_body(
            json!({ "match": { "level": "ERROR" } }),
            Some(("@timestamp", &window)),
            Some("@timestamp"),
            20,
        );
        assert_eq!(
            body,
            json!({
                "size": 20,
                "track_total_hits": true,
                "query": { "bool": {
                    "must": [{ "match": { "level": "ERROR" } }],
                    "filter": [{ "range": { "@timestamp": { "gte": "2026-09-24T00:00:00.000Z", "lte": "2026-09-25T00:00:00.000Z", "format": "strict_date_optional_time" } } }]
                } },
                "sort": [{ "@timestamp": { "order": "desc", "unmapped_type": "boolean" } }]
            })
        );
        let body = build_search_body(json!({ "match_all": {} }), None, None, 5);
        assert_eq!(body, json!({ "size": 5, "track_total_hits": true, "query": { "match_all": {} } }));
    }

    #[test]
    fn reads_totals_and_compacts_hits() {
        assert_eq!(
            read_total_hits(&json!({ "hits": { "total": { "value": 10000, "relation": "gte" } } })),
            (10000, "gte")
        );
        assert_eq!(read_total_hits(&json!({ "hits": { "total": 5 } })), (5, "eq"));
        assert_eq!(read_total_hits(&json!({})), (0, "eq"));
        let long = "x".repeat(MAX_VALUE_CHARS + 5);
        let response = json!({ "hits": { "hits": [
            { "_index": "logs-1", "_id": "a", "_source": { "@timestamp": "2026-09-26T10:00:00Z", "message": long } },
            { "_index": "logs-1", "_id": "b", "_source": { "event": { "created": "t2" } }, "sort": [1] },
            { "_index": "logs-2", "_id": "c", "_source": { "event.created": "t3" } }
        ] } });
        let hits = compact_hits(&response, Some("@timestamp"));
        assert_eq!(hits[0]["time"], "2026-09-26T10:00:00Z");
        assert!(hits[0]["_source"]["message"].as_str().unwrap().ends_with("[truncated 5 more chars]"));
        assert_eq!(hits[1]["time"], 1);
        let hits = compact_hits(&response, Some("event.created"));
        assert_eq!((hits[1]["time"].as_str(), hits[2]["time"].as_str()), (Some("t2"), Some("t3")));
        assert!(compact_hits(&response, None)[0].get("time").is_none());
    }

    #[test]
    fn parses_saved_index_patterns_and_dedupes_aliases() {
        let response = json!({ "hits": { "hits": [
            { "_index": ".kibana_1", "_id": "index-pattern:1", "_source": { "type": "index-pattern", "index-pattern": { "title": "logs-*", "timeFieldName": "@timestamp" } } },
            { "_index": ".kibana_1", "_id": "index-pattern:2", "_source": { "type": "index-pattern", "index-pattern": { "title": "customers", "timeFieldName": "" } } },
            { "_index": ".kibana", "_id": "index-pattern:1", "_source": { "type": "index-pattern", "index-pattern": { "title": "logs-*", "timeFieldName": "@timestamp" } } },
            { "_index": ".kibana_1", "_id": "config:1", "_source": { "type": "config" } }
        ] } });
        let patterns = parse_saved_index_patterns(&response);
        assert_eq!(patterns.len(), 2);
        assert_eq!(
            patterns[0],
            SavedIndexPattern {
                id: "2".into(),
                title: "customers".into(),
                time_field: None,
                saved_in: ".kibana_1".into()
            }
        );
        assert_eq!(patterns[1].time_field.as_deref(), Some("@timestamp"));
        assert_eq!(saved_index_patterns_request_body()["query"], json!({ "term": { "type": "index-pattern" } }));
    }

    #[test]
    fn derives_wildcard_bases() {
        assert_eq!(wildcard_base("logs-local-2026.09.20").as_deref(), Some("logs-local-*"));
        assert_eq!(wildcard_base("tx_2025_1").as_deref(), Some("tx_*"));
        assert_eq!(wildcard_base("app-000001").as_deref(), Some("app-*"));
        assert_eq!(wildcard_base("customers"), None);
        assert_eq!(wildcard_base("2026.09.20"), None);
    }

    #[test]
    fn summarizes_cat_indices() {
        let response = json!([
            { "index": "logs-2026.09.21", "health": "green", "status": "open", "docs.count": "12", "store.size": "1mb" },
            { "index": ".kibana_1", "health": "green", "status": "open", "docs.count": "3" },
            { "index": "logs-2026.09.20", "health": "yellow", "status": "open", "docs.count": null },
            { "index": "customers", "health": "green", "status": "open", "docs.count": "7" }
        ]);
        let (indices, hidden, derived) = summarize_cat_indices(&response);
        assert_eq!(hidden, 1);
        assert_eq!(
            indices.iter().map(|entry| entry["index"].as_str().unwrap()).collect::<Vec<_>>(),
            vec!["customers", "logs-2026.09.20", "logs-2026.09.21"]
        );
        assert_eq!(indices[2]["docs"], 12);
        assert!(indices[1].get("docs").is_none());
        assert_eq!(derived, vec![json!({ "pattern": "logs-*", "indices": 2 })]);
    }

    #[test]
    fn summarizes_resolve_index() {
        let response = json!({
            "indices": [{ "name": "logs-1", "aliases": ["logs"], "attributes": ["open"] }],
            "aliases": [{ "name": "logs", "indices": ["logs-1"] }],
            "data_streams": []
        });
        assert_eq!(
            summarize_resolve_index(&response),
            json!({ "indices": [{ "name": "logs-1", "aliases": ["logs"], "attributes": ["open"] }], "aliases": [{ "name": "logs", "indices": ["logs-1"] }], "data_streams": [] })
        );
    }

    #[test]
    fn parses_field_caps() {
        let response = json!({ "indices": ["a", "b"], "fields": {
            "_id": { "_id": { "type": "_id", "searchable": true, "aggregatable": false } },
            "_seq_no": { "_seq_no": { "type": "_seq_no", "searchable": true, "aggregatable": true } },
            "@timestamp": { "date": { "type": "date", "searchable": true, "aggregatable": true } },
            "kubernetes": { "object": { "type": "object", "searchable": false, "aggregatable": false } },
            "message": { "text": { "type": "text", "searchable": true, "aggregatable": false } },
            "payload": { "keyword": { "type": "keyword", "searchable": false, "aggregatable": false } },
            "status": {
                "long": { "type": "long", "searchable": true, "aggregatable": true, "indices": ["a"] },
                "keyword": { "type": "keyword", "searchable": true, "aggregatable": true, "indices": ["b"] }
            },
            "code": { "keyword": { "type": "keyword", "searchable": true, "aggregatable": true, "non_searchable_indices": ["b"] } }
        } });
        let fields = parse_field_caps(&response);
        let names: Vec<&str> = fields.iter().map(|field| field.name.as_str()).collect();
        assert_eq!(names, vec!["@timestamp", "_id", "code", "message", "payload", "status"]);
        assert!(fields[0].is_date());
        assert!(!fields[2].searchable, "partially searchable counts as not searchable");
        assert!(!fields[4].searchable);
        assert_eq!(fields[5].field_type, "conflict");
        assert_eq!(fields[5].to_json()["types"], json!(["keyword", "long"]));
        assert_eq!(
            fields[3].to_json(),
            json!({ "name": "message", "type": "text", "searchable": true, "aggregatable": false })
        );
        assert_eq!(known_dql_fields(&fields)[4], DqlKnownField { name: "payload".into(), searchable: false });
    }

    #[test]
    fn shapes_jdbc_and_elasticsearch_sql_responses() {
        let jdbc = json!({ "schema": [{ "name": "level", "type": "keyword" }, { "name": "count()", "alias": "c", "type": "integer" }], "datarows": [["ERROR", 3], ["WARN", 2], ["INFO", 1]], "total": 3, "size": 3, "status": 200 });
        let shaped = shape_tabular_response(&jdbc, 2).unwrap();
        assert_eq!(
            shaped["columns"],
            json!([{ "name": "level", "type": "keyword" }, { "name": "c", "type": "integer" }])
        );
        assert_eq!(shaped["rows"], json!([["ERROR", 3], ["WARN", 2]]));
        assert_eq!(
            (shaped["returned"].as_u64(), shaped["fetched"].as_u64(), shaped["total"].as_u64()),
            (Some(2), Some(3), Some(3))
        );
        assert!(shaped["notes"][0].as_str().unwrap().contains("first 2 of 3"));
        let es = json!({ "columns": [{ "name": "level", "type": "keyword" }], "rows": [["ERROR"]] });
        let shaped = shape_tabular_response(&es, 20).unwrap();
        assert_eq!(shaped["rows"], json!([["ERROR"]]));
        assert!(shaped.get("notes").is_none());
        assert!(shape_tabular_response(&json!({ "hits": {} }), 20).is_err());
    }

    #[test]
    fn extracts_error_reasons() {
        let body = r#"{"error":{"root_cause":[{"type":"query_shard_exception","reason":"failed to create query"}],"type":"search_phase_execution_exception","reason":"all shards failed","failed_shards":[{"reason":{"reason":"Cannot search on field [payload] since it is not indexed."}}]},"status":400}"#;
        let reason = error_reason(400, body);
        assert!(reason.starts_with("HTTP 400: all shards failed | failed to create query"), "{reason}");
        assert!(reason.contains("not indexed"), "{reason}");
        assert_eq!(error_reason(403, r#"{"error":"no permissions"}"#), "HTTP 403: no permissions");
        assert_eq!(error_reason(502, "Bad gateway"), "HTTP 502: Bad gateway");
    }

    #[test]
    fn caps_response_size_by_dropping_items() {
        let hits: Vec<Value> = (0..100).map(|index| json!({ "_id": index, "text": "y".repeat(1000) })).collect();
        let text = fit_response(json!({ "total": 100, "hits": hits }), "hits", 20 * 1024);
        assert!(text.len() <= 20 * 1024, "{}", text.len());
        let parsed: Value = serde_json::from_str(&text).unwrap();
        let kept = parsed["hits"].as_array().unwrap().len();
        assert!(kept > 5 && kept < 20, "{kept}");
        assert!(parsed["response_truncated"].as_str().unwrap().contains(&format!("showing {kept} of 100 hits")));
        let small = fit_response(json!({ "hits": [1, 2] }), "hits", 1024);
        assert_eq!(small, r#"{"hits":[1,2]}"#);
    }

    #[test]
    fn clamps_limits() {
        assert_eq!(clamp_limit(None), 20);
        assert_eq!(clamp_limit(Some(0)), 1);
        assert_eq!(clamp_limit(Some(500)), 200);
    }
}
