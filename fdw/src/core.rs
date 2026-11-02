//! Pure logic of the IRIS FDW: SQL generation, value conversion and response parsing.
//!
//! Nothing in this module depends on the Wasm host bindings, so it can be unit
//! tested natively with `cargo test`.

use serde_json::{Map, Value as JsonValue, json};

// ---------------------------------------------------------------------------
// Query parameters
// ---------------------------------------------------------------------------

/// A value bound to a `?` placeholder.
#[derive(Debug, Clone, PartialEq)]
pub enum Param {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
}

impl Param {
    pub fn to_json(&self) -> JsonValue {
        match self {
            Param::Null => JsonValue::Null,
            Param::Bool(b) => json!(b),
            Param::Int(n) => json!(n),
            Param::Float(f) => json!(f),
            Param::Str(s) => json!(s),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum QualValue {
    Single(Param),
    List(Vec<Param>),
}

/// A WHERE condition received from Postgres.
#[derive(Debug, Clone)]
pub struct QualSpec {
    pub field: String,
    pub operator: String,
    /// `None` when the value type cannot be converted (the qual is then not pushed down)
    pub value: Option<QualValue>,
    pub use_or: bool,
}

#[derive(Debug, Clone)]
pub struct SortSpec {
    pub field: String,
    pub reversed: bool,
    pub nulls_first: bool,
    /// text columns are never sorted remotely: IRIS default collation (SQLUPPER)
    /// differs from Postgres collations
    pub is_text: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    Table(String),
    Query(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScanPlan {
    pub sql: String,
    pub params: Vec<JsonValue>,
    /// maximum number of rows to fetch from IRIS (only when LIMIT is safely pushed down)
    pub max_rows: Option<usize>,
    /// size of the first page when the query has a LIMIT (never truncates results)
    pub first_page_hint: Option<usize>,
}

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

fn is_simple_ident(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '%')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Render a column name for IRIS SQL.
///
/// Names are always delimited: in IRIS delimited identifiers are still resolved
/// case-insensitively, and delimiting avoids any clash with the ~250 SQL reserved
/// words. Pseudo-columns such as `%ID` cannot be delimited and are left as is.
pub fn ident(name: &str) -> String {
    if name.starts_with('%') && is_simple_ident(name) {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

/// Validate and normalize a table name given in the foreign table options:
/// `Table`, `Schema.Table`, `"My Schema"."My Table"`. Each part must be a plain
/// identifier or a delimited one; nothing else (spaces, operators...) is accepted.
pub fn validate_table_name(name: &str) -> Result<String, String> {
    let err = || format!("invalid IRIS table name: '{name}'");
    let mut parts: Vec<String> = Vec::new();
    let mut chars = name.trim().chars().peekable();
    loop {
        let part = if chars.peek() == Some(&'"') {
            chars.next();
            let mut s = String::new();
            loop {
                match chars.next() {
                    Some('"') if chars.peek() == Some(&'"') => {
                        chars.next();
                        s.push('"');
                    }
                    Some('"') => break,
                    Some(c) if !c.is_control() => s.push(c),
                    _ => return Err(err()),
                }
            }
            if s.trim().is_empty() {
                return Err(err());
            }
            format!("\"{}\"", s.replace('"', "\"\""))
        } else {
            let mut s = String::new();
            while let Some(&c) = chars.peek() {
                if c == '.' {
                    break;
                }
                s.push(c);
                chars.next();
            }
            if !is_simple_ident(&s) {
                return Err(err());
            }
            s
        };
        parts.push(part);
        match chars.next() {
            None => break,
            Some('.') => continue,
            Some(_) => return Err(err()),
        }
    }
    if parts.is_empty() || parts.len() > 2 {
        return Err(err());
    }
    Ok(parts.join("."))
}

/// Validate a custom SELECT given with the `query` table option: one statement
/// (semicolons inside string literals or delimited identifiers are fine).
pub fn validate_query(query: &str) -> Result<String, String> {
    let q = query.trim().trim_end_matches(';').trim();
    let lower = q.to_ascii_lowercase();
    let starts_with_kw = |kw: &str| {
        lower.starts_with(kw)
            && lower[kw.len()..]
                .chars()
                .next()
                .map(|c| c.is_whitespace() || c == '(' || c == '*' || c == '%' || c == '"')
                .unwrap_or(false)
    };
    if !(starts_with_kw("select") || starts_with_kw("with")) {
        return Err("the 'query' option must be a SELECT statement".to_string());
    }
    let mut quote: Option<char> = None;
    for c in q.chars() {
        match (quote, c) {
            (None, '\'' | '"') => quote = Some(c),
            (Some(open), c) if c == open => quote = None,
            (None, ';') => {
                return Err("the 'query' option must contain a single statement".to_string());
            }
            _ => {}
        }
    }
    if quote.is_some() {
        return Err("the 'query' option has an unterminated quote".to_string());
    }
    Ok(q.to_string())
}

/// IRIS accepts at most 380 `?` parameters per statement: stay well below.
pub const MAX_IN_LIST: usize = 100;
pub const MAX_PARAMS: usize = 300;

// ---------------------------------------------------------------------------
// SELECT generation
// ---------------------------------------------------------------------------

/// Translate one qual into an IRIS SQL condition, or `None` if it cannot be
/// pushed down. Postgres re-checks every qual locally, so pushed conditions only
/// need to return a superset of the matching rows.
pub fn deparse_qual(q: &QualSpec) -> Option<(String, Vec<Param>)> {
    let field = ident(&q.field);
    match q.value.as_ref()? {
        QualValue::Single(param) => match (q.operator.as_str(), param) {
            ("is", Param::Str(s)) if s == "null" => Some((format!("{field} IS NULL"), vec![])),
            ("is not", Param::Str(s)) if s == "null" => {
                Some((format!("{field} IS NOT NULL"), vec![]))
            }
            ("is" | "is not", _) => None,
            (_, Param::Null) => None,
            ("=" | "<>" | "<" | "<=" | ">" | ">=", p) => {
                Some((format!("{field} {} ?", q.operator), vec![p.clone()]))
            }
            ("!=", p) => Some((format!("{field} <> ?"), vec![p.clone()])),
            ("~~", p @ Param::Str(_)) => Some((format!("{field} LIKE ? ESCAPE '\\'"), vec![p.clone()])),
            ("!~~", p @ Param::Str(_)) => {
                Some((format!("{field} NOT LIKE ? ESCAPE '\\'"), vec![p.clone()]))
            }
            _ => None,
        },
        QualValue::List(items) => {
            // long lists are filtered by Postgres instead (IRIS parameter limit)
            if items.is_empty() || items.len() > MAX_IN_LIST || items.iter().any(|p| *p == Param::Null) {
                return None;
            }
            let marks = vec!["?"; items.len()].join(", ");
            match (q.operator.as_str(), q.use_or) {
                // col = ANY(array)  <=>  col IN (...)
                ("=", true) => Some((format!("{field} IN ({marks})"), items.clone())),
                // col <> ALL(array) <=>  col NOT IN (...)
                ("<>", false) => Some((format!("{field} NOT IN ({marks})"), items.clone())),
                _ => None,
            }
        }
    }
}

pub struct SelectRequest<'a> {
    pub source: &'a Source,
    /// remote columns to fetch; `None` means `*`
    pub columns: Option<Vec<String>>,
    pub quals: &'a [QualSpec],
    pub sorts: &'a [SortSpec],
    /// (count, offset) from the Postgres query
    pub limit: Option<(i64, i64)>,
    pub limit_pushdown: bool,
}

pub fn build_select(req: &SelectRequest) -> ScanPlan {
    let mut conds = Vec::new();
    let mut params = Vec::new();
    let mut all_pushed = true;
    for q in req.quals {
        match deparse_qual(q) {
            Some((cond, p)) if params.len() + p.len() <= MAX_PARAMS => {
                conds.push(cond);
                params.extend(p);
            }
            _ => all_pushed = false,
        }
    }

    // Without truncation, LIMIT is still a safe hint for the size of the first page:
    // more pages are fetched if Postgres needs more rows.
    let first_page_hint = req
        .limit
        .filter(|(count, _)| *count >= 0)
        .map(|(count, offset)| (count + offset.max(0)).max(1) as usize);

    // Truncating with TOP is only safe when every condition was pushed down and the
    // ordering is reproduced exactly by IRIS. The wrapper host silently drops ORDER BY
    // items that are not plain columns of this table, so TOP also requires at least one
    // received sort key (an empty sort list may mean "ORDER BY <expression>"). The user
    // must also confirm the table is not joined (limit_pushdown option).
    let max_rows = match req.limit {
        Some((count, offset))
            if req.limit_pushdown
                && all_pushed
                && count >= 0
                && !req.sorts.is_empty()
                && req.sorts.iter().all(|s| !s.is_text) =>
        {
            Some((count + offset.max(0)) as usize)
        }
        _ => None,
    };

    let tgts = match &req.columns {
        None => "*".to_string(),
        Some(cols) if cols.is_empty() => "1 AS fdw_dummy".to_string(),
        Some(cols) => cols.iter().map(|c| ident(c)).collect::<Vec<_>>().join(", "),
    };
    let from = match req.source {
        Source::Table(t) => t.clone(),
        // the newline terminates a trailing `--` comment inside the user query
        Source::Query(q) => format!("({q}\n) fdw_q"),
    };

    let mut sql = String::from("SELECT ");
    if let Some(n) = max_rows {
        sql.push_str(&format!("TOP {n} "));
    }
    sql.push_str(&format!("{tgts} FROM {from}"));
    if !conds.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conds.join(" AND "));
    }
    // ORDER BY is only useful together with a pushed-down LIMIT: Postgres always
    // sorts the rows locally anyway.
    if max_rows.is_some() && !req.sorts.is_empty() {
        let order = req
            .sorts
            .iter()
            .map(|s| {
                let f = ident(&s.field);
                // emulate NULLS FIRST/LAST explicitly
                let nulls = if s.nulls_first {
                    format!("CASE WHEN {f} IS NULL THEN 0 ELSE 1 END")
                } else {
                    format!("CASE WHEN {f} IS NULL THEN 1 ELSE 0 END")
                };
                format!("{nulls}, {f} {}", if s.reversed { "DESC" } else { "ASC" })
            })
            .collect::<Vec<_>>()
            .join(", ");
        sql.push_str(&format!(" ORDER BY {order}"));
    }

    ScanPlan {
        sql,
        params: params.iter().map(Param::to_json).collect(),
        max_rows,
        first_page_hint,
    }
}

// ---------------------------------------------------------------------------
// INSERT / UPDATE / DELETE generation
// ---------------------------------------------------------------------------

pub fn build_insert(table: &str, cols: &[(String, Param)]) -> (String, Vec<JsonValue>) {
    if cols.is_empty() {
        return (format!("INSERT INTO {table} DEFAULT VALUES"), vec![]);
    }
    let names = cols.iter().map(|(c, _)| ident(c)).collect::<Vec<_>>().join(", ");
    let marks = vec!["?"; cols.len()].join(", ");
    (
        format!("INSERT INTO {table} ({names}) VALUES ({marks})"),
        cols.iter().map(|(_, p)| p.to_json()).collect(),
    )
}

pub fn build_update(
    table: &str,
    rowid_col: &str,
    rowid: &Param,
    cols: &[(String, Param)],
) -> Option<(String, Vec<JsonValue>)> {
    let cols: Vec<_> = cols
        .iter()
        .filter(|(c, _)| !c.eq_ignore_ascii_case(rowid_col))
        .collect();
    if cols.is_empty() {
        return None;
    }
    let mut params = Vec::new();
    let sets = cols
        .iter()
        .map(|(c, p)| {
            if *p == Param::Null {
                format!("{} = NULL", ident(c))
            } else {
                params.push(p.to_json());
                format!("{} = ?", ident(c))
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    params.push(rowid.to_json());
    Some((
        format!("UPDATE {table} SET {sets} WHERE {} = ?", ident(rowid_col)),
        params,
    ))
}

pub fn build_delete(table: &str, rowid_col: &str, rowid: &Param) -> (String, Vec<JsonValue>) {
    (
        format!("DELETE FROM {table} WHERE {} = ?", ident(rowid_col)),
        vec![rowid.to_json()],
    )
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

/// One page of rows returned by IRIS, normalized for both API modes.
#[derive(Debug, Default, Clone)]
pub struct Page {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<JsonValue>>,
    pub has_more: bool,
}

/// Extract an error message from an IRIS response body, if any.
pub fn response_error(body: &JsonValue) -> Option<String> {
    // gateway: {"error": {"message": "...", "sqlcode": -30}}
    if let Some(err) = body.get("error") {
        let msg = err.get("message").and_then(|m| m.as_str()).unwrap_or("unknown error");
        return Some(match err.get("sqlcode").and_then(|c| c.as_i64()) {
            Some(code) => format!("IRIS SQLCODE {code}: {msg}"),
            None => format!("IRIS error: {msg}"),
        });
    }
    // Atelier API: {"status": {"errors": [{"error": "..."}]}}
    if let Some(errors) = body.pointer("/status/errors").and_then(|e| e.as_array()) {
        if !errors.is_empty() {
            let msgs = errors
                .iter()
                .map(|e| e.get("error").and_then(|m| m.as_str()).unwrap_or("unknown error").to_string())
                .collect::<Vec<_>>()
                .join("; ");
            return Some(format!("IRIS error: {msgs}"));
        }
    }
    None
}

pub fn parse_gateway_page(body: &JsonValue) -> Result<Page, String> {
    let columns = body
        .get("columns")
        .and_then(|c| c.as_array())
        .ok_or("invalid gateway response: missing 'columns'")?
        .iter()
        .map(|c| c.get("name").and_then(|n| n.as_str()).unwrap_or_default().to_string())
        .collect();
    let rows = body
        .get("rows")
        .and_then(|r| r.as_array())
        .ok_or("invalid gateway response: missing 'rows'")?
        .iter()
        .map(|r| r.as_array().cloned().unwrap_or_default())
        .collect();
    let has_more = body.get("hasMore").and_then(|h| h.as_bool()).unwrap_or(false);
    Ok(Page { columns, rows, has_more })
}

/// Atelier returns rows as JSON objects: convert them to the column/row layout.
pub fn parse_atelier_page(body: &JsonValue) -> Result<Page, String> {
    let content = body
        .pointer("/result/content")
        .and_then(|c| c.as_array())
        .ok_or("invalid Atelier response: missing 'result.content'")?;
    let columns: Vec<String> = match content.first().and_then(|r| r.as_object()) {
        Some(obj) => obj.keys().cloned().collect(),
        None => vec![],
    };
    let rows = content
        .iter()
        .map(|r| {
            columns
                .iter()
                .map(|c| r.get(c).cloned().unwrap_or(JsonValue::Null))
                .collect()
        })
        .collect();
    Ok(Page { columns, rows, has_more: false })
}

/// Map each target column to its index in the IRIS result set (case-insensitive).
pub fn map_columns(targets: &[String], source: &[String]) -> Result<Vec<Option<usize>>, String> {
    targets
        .iter()
        .map(|t| {
            if t == "_attrs" {
                return Ok(None);
            }
            source
                .iter()
                .position(|s| s.eq_ignore_ascii_case(t))
                .map(Some)
                .ok_or_else(|| {
                    format!(
                        "column '{t}' not found in IRIS result set (available: {})",
                        source.join(", ")
                    )
                })
        })
        .collect()
}

/// Build the `_attrs` JSON object holding the whole IRIS row.
pub fn row_to_attrs(columns: &[String], row: &[JsonValue]) -> String {
    let mut obj = Map::new();
    for (c, v) in columns.iter().zip(row.iter()) {
        obj.insert(c.clone(), v.clone());
    }
    JsonValue::Object(obj).to_string()
}

// ---------------------------------------------------------------------------
// Scalar conversions
// ---------------------------------------------------------------------------

pub fn json_to_bool(v: &JsonValue) -> Option<bool> {
    match v {
        JsonValue::Bool(b) => Some(*b),
        JsonValue::Number(n) => n.as_f64().map(|f| f != 0.0),
        JsonValue::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "t" | "yes" | "y" => Some(true),
            "0" | "false" | "f" | "no" | "n" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

pub fn json_to_i64(v: &JsonValue) -> Option<i64> {
    match v {
        JsonValue::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        JsonValue::String(s) => {
            let s = s.trim();
            s.parse::<i64>().ok().or_else(|| s.parse::<f64>().ok().map(|f| f as i64))
        }
        JsonValue::Bool(b) => Some(*b as i64),
        _ => None,
    }
}

pub fn json_to_f64(v: &JsonValue) -> Option<f64> {
    match v {
        JsonValue::Number(n) => n.as_f64(),
        JsonValue::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
    // NaN / infinity cannot be represented as Postgres numeric
    .filter(|f| f.is_finite())
}

/// Integer conversion with range check (no silent truncation).
pub fn json_to_int<T: TryFrom<i64>>(v: &JsonValue) -> Option<T> {
    json_to_i64(v).and_then(|n| T::try_from(n).ok())
}

pub fn json_to_string(v: &JsonValue) -> String {
    match v {
        JsonValue::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Value for a jsonb column: JSON strings holding JSON documents are parsed.
pub fn json_to_json_text(v: &JsonValue) -> String {
    match v {
        JsonValue::String(s) => match serde_json::from_str::<JsonValue>(s) {
            Ok(parsed @ (JsonValue::Object(_) | JsonValue::Array(_))) => parsed.to_string(),
            _ => JsonValue::String(s.clone()).to_string(),
        },
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Dates and timestamps (proleptic Gregorian calendar, UTC)
// ---------------------------------------------------------------------------

/// Days since 1970-01-01 (algorithm by Howard Hinnant).
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn num(s: &str) -> Option<i64> {
    if s.is_empty() || !s.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// Parse `YYYY-MM-DD` (anything after the date is ignored) to days since epoch.
pub fn parse_date_days(s: &str) -> Option<i64> {
    let s = s.trim();
    let date = s.get(0..10)?;
    let mut it = date.split('-');
    let (y, m, d) = (num(it.next()?)?, num(it.next()?)?, num(it.next()?)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, m, d))
}

/// Parse `YYYY-MM-DD[ T]HH:MM[:SS[.ffffff]][Z|±HH[:MM]]` to microseconds since
/// epoch (UTC when an offset is present).
pub fn parse_timestamp_micros(s: &str) -> Option<i64> {
    let s = s.trim().trim_end_matches('Z');
    // split a trailing UTC offset, searching after the date part
    let (s, offset_secs) = match s.get(10..).and_then(|t| t.rfind(['+', '-'])) {
        Some(pos) => {
            let (base, off) = s.split_at(10 + pos);
            let sign = if off.starts_with('-') { -1 } else { 1 };
            let digits: String = off[1..].chars().filter(|c| *c != ':').collect();
            if digits.len() != 2 && digits.len() != 4 {
                return None;
            }
            let h = num(&digits[0..2])?;
            let m = if digits.len() == 4 { num(&digits[2..4])? } else { 0 };
            (base, sign * (h * 3600 + m * 60))
        }
        None => (s, 0),
    };
    Some(parse_naive_timestamp_micros(s)? - offset_secs * 1_000_000)
}

/// Decode the logical value of `%Library.PosixTime` (returned by the Atelier API)
/// to microseconds since epoch.
pub fn posix_logical_to_micros(v: i64) -> i64 {
    const BASE: i64 = 1 << 60;
    if v >= 0 { v - BASE } else { v + 6 * BASE }
}

/// Parse a timestamp returned by IRIS: an ISO string (ODBC mode, %TimeStamp in
/// logical mode) or the 64-bit logical value of %PosixTime (Atelier API).
pub fn parse_iris_timestamp_micros(v: &JsonValue) -> Option<i64> {
    match v {
        JsonValue::Number(n) => n.as_i64().map(posix_logical_to_micros),
        JsonValue::String(s) => {
            let t = s.trim();
            let digits = t.strip_prefix('-').unwrap_or(t);
            if digits.len() >= 15 && digits.chars().all(|c| c.is_ascii_digit()) {
                t.parse::<i64>().ok().map(posix_logical_to_micros)
            } else {
                parse_timestamp_micros(t)
            }
        }
        _ => None,
    }
}

fn parse_naive_timestamp_micros(s: &str) -> Option<i64> {
    let days = parse_date_days(s)?;
    let mut micros = days * 86_400_000_000;
    if let Some(time) = s.get(11..) {
        let (hms, frac) = match time.split_once('.') {
            Some((a, b)) => (a, Some(b)),
            None => (time, None),
        };
        let mut parts = hms.split(':');
        let h = num(parts.next()?)?;
        let mi = num(parts.next().unwrap_or("0"))?;
        let sec = num(parts.next().unwrap_or("0"))?;
        micros += ((h * 60 + mi) * 60 + sec) * 1_000_000;
        if let Some(frac) = frac {
            let digits: String = frac.chars().take_while(|c| c.is_ascii_digit()).take(6).collect();
            if !digits.is_empty() {
                let scale = 10_i64.pow(6 - digits.len() as u32);
                micros += num(&digits)? * scale;
            }
        }
    }
    Some(micros)
}

/// Days between the IRIS $HOROLOG epoch (1840-12-31) and the Unix epoch.
pub const HOROLOG_UNIX_OFFSET: i64 = 47117;

/// Parse a date returned by IRIS: ISO `YYYY-MM-DD` (ODBC mode) or a $HOROLOG
/// day number (logical mode, used by the Atelier API). Returns days since epoch.
pub fn parse_iris_date_days(v: &JsonValue) -> Option<i64> {
    match v {
        JsonValue::Number(n) => n.as_i64().map(|h| h - HOROLOG_UNIX_OFFSET),
        JsonValue::String(s) => {
            let s = s.trim();
            if !s.is_empty() && s.chars().all(|c| c.is_ascii_digit() || c == '-') && !s.contains("--") && s.len() < 8 {
                s.parse::<i64>().ok().map(|h| h - HOROLOG_UNIX_OFFSET)
            } else {
                parse_date_days(s)
            }
        }
        _ => None,
    }
}

pub fn format_date_secs(secs: i64) -> String {
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

pub fn format_timestamp_micros(micros: i64) -> String {
    let days = micros.div_euclid(86_400_000_000);
    let rem = micros.rem_euclid(86_400_000_000);
    let (y, m, d) = civil_from_days(days);
    let secs = rem / 1_000_000;
    let frac = rem % 1_000_000;
    let base = format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    );
    if frac == 0 { base } else { format!("{base}.{frac:06}") }
}

// ---------------------------------------------------------------------------
// IMPORT FOREIGN SCHEMA
// ---------------------------------------------------------------------------

pub const IMPORT_COLUMNS_SQL: &str = "SELECT TABLE_NAME, COLUMN_NAME, DATA_TYPE, PRIMARY_KEY \
     FROM INFORMATION_SCHEMA.COLUMNS WHERE TABLE_SCHEMA = ? \
     ORDER BY TABLE_NAME, ORDINAL_POSITION";

/// Map an IRIS SQL data type (INFORMATION_SCHEMA.COLUMNS.DATA_TYPE) to Postgres.
pub fn pg_type_for_iris(data_type: &str) -> &'static str {
    match data_type.trim().to_ascii_lowercase().as_str() {
        "bit" | "boolean" => "boolean",
        "tinyint" | "smallint" => "smallint",
        "integer" | "int" => "integer",
        "bigint" => "bigint",
        "numeric" | "decimal" | "money" | "smallmoney" => "numeric",
        "double" | "float" | "real" | "double precision" => "double precision",
        "date" => "date",
        "timestamp" | "datetime" | "smalldatetime" | "posixtime" | "datetime2" => "timestamp",
        _ => "text",
    }
}

#[derive(Debug, Clone)]
pub struct ImportColumn {
    pub table: String,
    pub column: String,
    pub data_type: String,
    pub primary_key: bool,
}

/// Read the rows of IMPORT_COLUMNS_SQL by column name (Atelier returns JSON
/// objects, so the column order must not be relied upon).
pub fn import_columns_from_page(page: &Page) -> Result<Vec<ImportColumn>, String> {
    let names: Vec<String> = ["TABLE_NAME", "COLUMN_NAME", "DATA_TYPE", "PRIMARY_KEY"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    if page.rows.is_empty() {
        return Ok(vec![]);
    }
    let idx = map_columns(&names, &page.columns)?;
    let get = |row: &[JsonValue], i: usize| {
        idx[i].and_then(|k| row.get(k)).map(json_to_string).unwrap_or_default()
    };
    Ok(page
        .rows
        .iter()
        .map(|r| ImportColumn {
            table: get(r, 0),
            column: get(r, 1),
            data_type: get(r, 2),
            primary_key: matches!(get(r, 3).to_ascii_uppercase().as_str(), "YES" | "1" | "TRUE"),
        })
        .collect())
}

/// Schema or table name for the `table` option: plain names stay undelimited.
fn table_part(name: &str) -> String {
    if is_simple_ident(name) {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

fn pg_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn pg_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Generate `CREATE FOREIGN TABLE` statements for the selected IRIS tables.
/// Postgres table and column names are lowercased so they can be used unquoted.
pub fn build_import_statements(
    server: &str,
    local_schema: &str,
    remote_schema: &str,
    columns: &[ImportColumn],
    include: impl Fn(&str) -> bool,
) -> Vec<String> {
    let mut stmts = Vec::new();
    let mut i = 0;
    while i < columns.len() {
        let table = &columns[i].table;
        let mut j = i;
        while j < columns.len() && &columns[j].table == table {
            j += 1;
        }
        if include(table) {
            let cols = &columns[i..j];
            let defs = cols
                .iter()
                .map(|c| format!("{} {}", pg_ident(&c.column.to_lowercase()), pg_type_for_iris(&c.data_type)))
                .collect::<Vec<_>>()
                .join(", ");
            // row id: the IRIS RowID column "ID", or else a single-column primary key
            let pk: Vec<_> = cols.iter().filter(|c| c.primary_key).collect();
            let rowid = cols
                .iter()
                .find(|c| c.column.eq_ignore_ascii_case("id"))
                .or(if pk.len() == 1 { Some(pk[0]) } else { None })
                .map(|c| format!(", rowid_column {}", pg_literal(&c.column.to_lowercase())))
                .unwrap_or_default();
            let remote = format!("{}.{}", table_part(remote_schema), table_part(table));
            stmts.push(format!(
                "CREATE FOREIGN TABLE IF NOT EXISTS {}.{} ({defs}) SERVER {} OPTIONS (table {}{rowid})",
                pg_ident(local_schema),
                pg_ident(&table.to_lowercase()),
                pg_ident(server),
                pg_literal(&remote),
            ));
        }
        i = j;
    }
    stmts
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn qual(field: &str, op: &str, v: QualValue, use_or: bool) -> QualSpec {
        QualSpec { field: field.into(), operator: op.into(), value: Some(v), use_or }
    }

    fn select(
        source: Source,
        columns: Option<Vec<String>>,
        quals: &[QualSpec],
        sorts: &[SortSpec],
        limit: Option<(i64, i64)>,
        limit_pushdown: bool,
    ) -> ScanPlan {
        build_select(&SelectRequest { source: &source, columns, quals, sorts, limit, limit_pushdown })
    }

    #[test]
    fn identifiers() {
        assert_eq!(ident("name"), "\"name\"");
        assert_eq!(ident("role"), "\"role\"");
        assert_eq!(ident("my col"), "\"my col\"");
        assert_eq!(ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(ident("%ID"), "%ID");
    }

    #[test]
    fn table_names() {
        assert_eq!(validate_table_name("Demo.Patient").unwrap(), "Demo.Patient");
        assert_eq!(validate_table_name(" Patient ").unwrap(), "Patient");
        assert_eq!(
            validate_table_name("\"My Schema\".\"My \"\"T\"\"\"").unwrap(),
            "\"My Schema\".\"My \"\"T\"\"\""
        );
        for bad in [
            "",
            "Demo.Patient; DROP TABLE x",
            "Demo.Patient UNION SELECT Name FROM Other",
            "Demo.Patient WHERE 1=1 OR id",
            "a.b.c",
            "Demo.",
            "\"unterminated",
            "\"\"",
            "Demo.Patient--",
        ] {
            assert!(validate_table_name(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn queries() {
        assert_eq!(validate_query("select * from Demo.Patient;").unwrap(), "select * from Demo.Patient");
        assert!(validate_query("SELECT * FROM t WHERE notes LIKE '%;%'").is_ok());
        assert!(validate_query("select\n* from t").is_ok());
        assert!(validate_query("with x as (select 1) select * from x").is_ok());
        assert!(validate_query("delete from Demo.Patient").is_err());
        assert!(validate_query("selectx from t").is_err());
        assert!(validate_query("select 1; delete from x").is_err());
        assert!(validate_query("select 'oops").is_err());
    }

    #[test]
    fn quals() {
        let q = qual("city", "=", QualValue::Single(Param::Str("Roma".into())), false);
        assert_eq!(deparse_qual(&q).unwrap().0, "\"city\" = ?");
        let q = qual("name", "~~", QualValue::Single(Param::Str("A%".into())), false);
        assert_eq!(deparse_qual(&q).unwrap().0, "\"name\" LIKE ? ESCAPE '\\'");
        let q = qual("email", "is", QualValue::Single(Param::Str("null".into())), false);
        assert_eq!(deparse_qual(&q).unwrap(), ("\"email\" IS NULL".into(), vec![]));
        let q = qual("id", "=", QualValue::List(vec![Param::Int(1), Param::Int(2)]), true);
        assert_eq!(deparse_qual(&q).unwrap().0, "\"id\" IN (?, ?)");
        let q = qual("id", "<>", QualValue::List(vec![Param::Int(1)]), false);
        assert_eq!(deparse_qual(&q).unwrap().0, "\"id\" NOT IN (?)");
        // `id = ALL(array)` cannot be pushed down
        let q = qual("id", "=", QualValue::List(vec![Param::Int(1)]), false);
        assert!(deparse_qual(&q).is_none());
        let q = qual("name", "~~*", QualValue::Single(Param::Str("a%".into())), false);
        assert!(deparse_qual(&q).is_none());
        // lists over the IRIS parameter limit are filtered by Postgres
        let big = (0..(MAX_IN_LIST as i64 + 1)).map(Param::Int).collect();
        assert!(deparse_qual(&qual("id", "=", QualValue::List(big), true)).is_none());
    }

    #[test]
    fn select_without_limit_pushdown() {
        let quals = vec![
            qual("city", "=", QualValue::Single(Param::Str("Roma".into())), false),
            qual("riskscore", ">", QualValue::Single(Param::Float(50.0)), false),
        ];
        let sorts = vec![SortSpec { field: "id".into(), reversed: false, nulls_first: false, is_text: false }];
        let plan = select(
            Source::Table("Demo.Patient".into()),
            Some(vec!["id".into(), "name".into()]),
            &quals,
            &sorts,
            Some((10, 0)),
            false,
        );
        assert_eq!(
            plan.sql,
            "SELECT \"id\", \"name\" FROM Demo.Patient WHERE \"city\" = ? AND \"riskscore\" > ?"
        );
        assert_eq!(plan.params, vec![json!("Roma"), json!(50.0)]);
        assert_eq!(plan.max_rows, None);
    }

    #[test]
    fn select_param_budget() {
        // three lists of 100 values fit, the fourth is left to Postgres
        let list = || QualValue::List((0..100).map(Param::Int).collect());
        let quals: Vec<_> = ["a", "b", "c", "d"].iter().map(|f| qual(f, "=", list(), true)).collect();
        let plan = select(Source::Table("T".into()), None, &quals, &[], Some((1, 0)), true);
        assert_eq!(plan.params.len(), 300);
        assert!(!plan.sql.contains("\"d\""));
        assert_eq!(plan.max_rows, None, "LIMIT must not be pushed when a qual is left out");
    }

    #[test]
    fn select_with_limit_pushdown() {
        let sorts = vec![SortSpec { field: "birthdate".into(), reversed: true, nulls_first: true, is_text: false }];
        let plan = select(
            Source::Table("Demo.Patient".into()),
            Some(vec!["id".into()]),
            &[],
            &sorts,
            Some((10, 5)),
            true,
        );
        assert_eq!(
            plan.sql,
            "SELECT TOP 15 \"id\" FROM Demo.Patient ORDER BY CASE WHEN \"birthdate\" IS NULL THEN 0 ELSE 1 END, \"birthdate\" DESC"
        );
        assert_eq!(plan.max_rows, Some(15));
        assert_eq!(plan.first_page_hint, Some(15));

        // no received sort key: the query may be ordered by an expression that the
        // host did not pass down (`ORDER BY round(x)`), so TOP must not be used;
        // the LIMIT only sizes the first page
        let plan = select(Source::Table("Demo.Patient".into()), Some(vec!["id".into()]), &[], &[], Some((10, 0)), true);
        assert_eq!(plan.sql, "SELECT \"id\" FROM Demo.Patient");
        assert_eq!(plan.max_rows, None);
        assert_eq!(plan.first_page_hint, Some(10));

        // a non pushable qual disables LIMIT pushdown
        let quals = vec![qual("name", "~~*", QualValue::Single(Param::Str("a%".into())), false)];
        let plan = select(Source::Table("Demo.Patient".into()), None, &quals, &[], Some((10, 0)), true);
        assert_eq!(plan.sql, "SELECT * FROM Demo.Patient");
        assert_eq!(plan.max_rows, None);

        // text sorts disable LIMIT pushdown (collation differences)
        let sorts = vec![SortSpec { field: "name".into(), reversed: false, nulls_first: false, is_text: true }];
        let plan = select(
            Source::Query("select * from T -- note".into()),
            Some(vec![]),
            &[],
            &sorts,
            Some((10, 0)),
            true,
        );
        assert_eq!(plan.sql, "SELECT 1 AS fdw_dummy FROM (select * from T -- note\n) fdw_q");
        assert_eq!(plan.max_rows, None);
    }

    #[test]
    fn modify_statements() {
        let cols = vec![("name".to_string(), Param::Str("Anna".into())), ("city".to_string(), Param::Null)];
        let (sql, params) = build_insert("Demo.Patient", &cols);
        assert_eq!(sql, "INSERT INTO Demo.Patient (\"name\", \"city\") VALUES (?, ?)");
        assert_eq!(params, vec![json!("Anna"), JsonValue::Null]);

        let cols = vec![
            ("id".to_string(), Param::Int(7)),
            ("name".to_string(), Param::Str("Anna".into())),
            ("city".to_string(), Param::Null),
        ];
        let (sql, params) = build_update("Demo.Patient", "id", &Param::Int(7), &cols).unwrap();
        assert_eq!(sql, "UPDATE Demo.Patient SET \"name\" = ?, \"city\" = NULL WHERE \"id\" = ?");
        assert_eq!(params, vec![json!("Anna"), json!(7)]);

        let (sql, _) = build_delete("Demo.Patient", "id", &Param::Int(7));
        assert_eq!(sql, "DELETE FROM Demo.Patient WHERE \"id\" = ?");
    }

    #[test]
    fn dates() {
        assert_eq!(parse_date_days("1970-01-01"), Some(0));
        assert_eq!(parse_date_days("2000-03-01"), Some(11017));
        assert_eq!(parse_date_days("1950-01-01"), Some(-7305));
        assert_eq!(parse_date_days("garbage"), None);
        assert_eq!(parse_iris_date_days(&json!("2000-03-01")), Some(11017));
        assert_eq!(parse_iris_date_days(&json!("47117")), Some(0));
        assert_eq!(parse_iris_date_days(&json!(58134)), Some(11017));
        assert_eq!(parse_timestamp_micros("1970-01-02 00:00:01"), Some(86_401_000_000));
        assert_eq!(parse_timestamp_micros("1970-01-01T00:00:00.5Z"), Some(500_000));
        assert_eq!(parse_timestamp_micros("1970-01-01 00:00:00.123"), Some(123_000));
        assert_eq!(parse_timestamp_micros("1970-01-01T01:00:00+01:00"), Some(0));
        assert_eq!(parse_timestamp_micros("1970-01-01T00:00:00-0230"), Some(9_000_000_000));
        assert_eq!(parse_timestamp_micros("1970-01-01 00:00:00+1"), None);
        assert_eq!(format_date_secs(11017 * 86_400), "2000-03-01");
        assert_eq!(format_date_secs(-7305 * 86_400), "1950-01-01");
        assert_eq!(format_timestamp_micros(86_401_000_000), "1970-01-02 00:00:01");
        assert_eq!(format_timestamp_micros(500_000), "1970-01-01 00:00:00.500000");
        for days in [-100_000, -1, 0, 1, 19_000, 60_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
    }

    #[test]
    fn posix_time() {
        // values produced by ##class(%Library.PosixTime).OdbcToLogical() on IRIS 2026.1
        assert_eq!(posix_logical_to_micros(1152921504606846976), 0);
        assert_eq!(parse_iris_timestamp_micros(&json!(1152921504606846976_i64)), Some(0));
        assert_eq!(parse_iris_timestamp_micros(&json!("1152921504606846976")), Some(0));
        assert_eq!(parse_iris_timestamp_micros(&json!("1970-01-01 00:00:01")), Some(1_000_000));
        assert_eq!(
            format_timestamp_micros(posix_logical_to_micros(1154715119436970432)),
            "2026-11-02 10:20:30.123456"
        );
        assert_eq!(
            format_timestamp_micros(posix_logical_to_micros(-6917844646841081856)),
            "1960-01-01 00:00:00"
        );
    }

    #[test]
    fn conversions() {
        assert_eq!(json_to_bool(&json!(1)), Some(true));
        assert_eq!(json_to_bool(&json!("0")), Some(false));
        assert_eq!(json_to_i64(&json!("42")), Some(42));
        assert_eq!(json_to_int::<i16>(&json!(70000)), None);
        assert_eq!(json_to_int::<i32>(&json!(70000)), Some(70000));
        assert_eq!(json_to_f64(&json!(".5")), Some(0.5));
        assert_eq!(json_to_f64(&json!("NaN")), None);
        assert_eq!(json_to_f64(&json!("inf")), None);
        assert_eq!(json_to_json_text(&json!("{\"a\":1}")), "{\"a\":1}");
        assert_eq!(json_to_json_text(&json!("plain")), "\"plain\"");
    }

    #[test]
    fn responses() {
        let body = json!({"columns": [{"name": "ID"}, {"name": "Name"}], "rows": [[1, "Anna"]], "hasMore": true});
        let page = parse_gateway_page(&body).unwrap();
        assert_eq!(page.columns, vec!["ID", "Name"]);
        assert!(page.has_more);
        let map = map_columns(&["name".into(), "_attrs".into(), "id".into()], &page.columns).unwrap();
        assert_eq!(map, vec![Some(1), None, Some(0)]);
        assert!(map_columns(&["nope".into()], &page.columns).is_err());
        assert_eq!(row_to_attrs(&page.columns, &page.rows[0]), r#"{"ID":1,"Name":"Anna"}"#);

        let body = json!({"status": {"errors": []}, "result": {"content": [{"ID": 1, "Name": "Anna"}]}});
        let page = parse_atelier_page(&body).unwrap();
        assert_eq!(page.columns, vec!["ID", "Name"]);
        assert_eq!(page.rows, vec![vec![json!(1), json!("Anna")]]);

        assert_eq!(
            response_error(&json!({"error": {"message": "Table not found", "sqlcode": -30}})),
            Some("IRIS SQLCODE -30: Table not found".into())
        );
        assert_eq!(
            response_error(&json!({"status": {"errors": [{"error": "boom"}]}})),
            Some("IRIS error: boom".into())
        );
        assert_eq!(response_error(&json!({"status": {"errors": []}})), None);
    }

    fn col(table: &str, column: &str, data_type: &str, pk: bool) -> ImportColumn {
        ImportColumn { table: table.into(), column: column.into(), data_type: data_type.into(), primary_key: pk }
    }

    #[test]
    fn import_statements() {
        let cols = vec![
            col("Appointment", "ID", "integer", true),
            col("Appointment", "StartsAt", "timestamp", false),
            col("DdlItem", "Code", "varchar", true),
            col("DdlItem", "Label", "varchar", false),
            col("My View", "Total", "numeric", false),
            col("Patient", "ID", "bigint", true),
            col("Patient", "Name", "varchar", false),
        ];
        let stmts = build_import_statements("iris_server", "iris", "Demo", &cols, |t| t != "Appointment");
        assert_eq!(stmts.len(), 3);
        assert_eq!(
            stmts[0],
            "CREATE FOREIGN TABLE IF NOT EXISTS \"iris\".\"ddlitem\" (\"code\" text, \"label\" text) \
             SERVER \"iris_server\" OPTIONS (table 'Demo.DdlItem', rowid_column 'code')"
        );
        assert_eq!(
            stmts[1],
            "CREATE FOREIGN TABLE IF NOT EXISTS \"iris\".\"my view\" (\"total\" numeric) \
             SERVER \"iris_server\" OPTIONS (table 'Demo.\"My View\"')"
        );
        assert_eq!(
            stmts[2],
            "CREATE FOREIGN TABLE IF NOT EXISTS \"iris\".\"patient\" (\"id\" bigint, \"name\" text) \
             SERVER \"iris_server\" OPTIONS (table 'Demo.Patient', rowid_column 'id')"
        );
        // the generated table option is accepted by the validator
        assert!(validate_table_name("Demo.\"My View\"").is_ok());
    }

    #[test]
    fn import_rows_by_name() {
        // Atelier rows are objects: column order is not guaranteed
        let page = Page {
            columns: vec!["PRIMARY_KEY".into(), "DATA_TYPE".into(), "COLUMN_NAME".into(), "TABLE_NAME".into()],
            rows: vec![vec![json!("YES"), json!("bigint"), json!("ID"), json!("Patient")]],
            has_more: false,
        };
        let cols = import_columns_from_page(&page).unwrap();
        assert_eq!(cols[0].table, "Patient");
        assert_eq!(cols[0].column, "ID");
        assert_eq!(cols[0].data_type, "bigint");
        assert!(cols[0].primary_key);
    }
}
