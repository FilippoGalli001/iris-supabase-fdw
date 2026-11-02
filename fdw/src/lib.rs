//! Supabase Wasm foreign data wrapper for InterSystems IRIS.
//!
//! Wasm FDWs can only talk HTTP(S)/JSON, so this wrapper sends parameterized SQL
//! to IRIS through either:
//! * the `iris-fdw-gateway` REST package (`api_mode 'gateway'`, default), or
//! * the built-in Atelier REST API (`api_mode 'atelier'`, zero install).

pub mod core;

#[cfg(target_arch = "wasm32")]
#[allow(warnings)]
mod bindings;

#[cfg(target_arch = "wasm32")]
mod fdw {
    use crate::bindings::{
        exports::supabase::wrappers::routines::Guest,
        supabase::wrappers::{
            http, stats,
            types::{
                Cell, Context, FdwError, FdwResult, ImportForeignSchemaStmt, ImportSchemaType,
                OptionsType, Row, TypeOid, Value,
            },
            utils,
        },
    };
    use crate::core::*;
    use base64::Engine;
    use serde_json::{Value as JsonValue, json};

    static FDW_NAME: &str = "IrisFdw";

    #[derive(Debug, Default, Clone, Copy, PartialEq)]
    enum ApiMode {
        #[default]
        Gateway,
        Atelier,
    }

    #[derive(Debug, Default)]
    struct IrisFdw {
        // connection
        mode: ApiMode,
        base_url: String,
        gateway_path: String,
        namespace: String,
        headers: Vec<(String, String)>,
        batch_size: usize,
        limit_pushdown: bool,
        debug: bool,

        // scan state
        plan: Option<ScanPlan>,
        page: Page,
        page_idx: usize,
        fetched: usize,
        col_map: Vec<Option<usize>>,

        // modify state
        table: String,
        rowid_col: String,
    }

    static mut INSTANCE: *mut IrisFdw = std::ptr::null_mut::<IrisFdw>();

    impl IrisFdw {
        fn init_instance() {
            let instance = Self::default();
            unsafe {
                INSTANCE = Box::leak(Box::new(instance));
            }
        }

        fn this_mut() -> &'static mut Self {
            unsafe { &mut (*INSTANCE) }
        }

        // -------------------------------------------------------------------
        // HTTP
        // -------------------------------------------------------------------

        fn endpoint(&self, op: &str) -> String {
            match self.mode {
                ApiMode::Gateway => format!("{}{}/{op}", self.base_url, self.gateway_path),
                ApiMode::Atelier => format!(
                    "{}/api/atelier/v1/{}/action/query",
                    self.base_url,
                    // e.g. %SYS must be sent as %25SYS
                    self.namespace.replace('%', "%25")
                ),
            }
        }

        fn post(&self, url: String, body: JsonValue) -> Result<JsonValue, FdwError> {
            let body = body.to_string();
            stats::inc_stats(FDW_NAME, stats::Metric::BytesOut, body.len() as i64);
            let req = http::Request {
                method: http::Method::Post,
                url: url.clone(),
                headers: self.headers.clone(),
                body,
            };
            let resp = http::post(&req).map_err(|e| format!("HTTP request to IRIS failed: {e}"))?;
            stats::inc_stats(FDW_NAME, stats::Metric::BytesIn, resp.body.len() as i64);

            if resp.status_code == 401 {
                return Err(format!(
                    "IRIS rejected the credentials (HTTP 401) at {url}: check username/password"
                ));
            }
            let json: JsonValue = match serde_json::from_str(&resp.body) {
                Ok(v) => v,
                Err(_) => {
                    let snippet: String = resp.body.chars().take(300).collect();
                    return Err(format!(
                        "unexpected response from IRIS (HTTP {}) at {url}: {snippet}",
                        resp.status_code
                    ));
                }
            };
            if let Some(err) = response_error(&json) {
                return Err(err);
            }
            if resp.status_code >= 400 {
                return Err(format!("IRIS returned HTTP {} at {url}", resp.status_code));
            }
            Ok(json)
        }

        /// Run a SELECT and return one page of rows.
        fn query_page(
            &self,
            sql: &str,
            params: &[JsonValue],
            offset: usize,
            limit: usize,
        ) -> Result<Page, FdwError> {
            match self.mode {
                ApiMode::Gateway => {
                    let body = json!({"sql": sql, "params": params, "offset": offset, "limit": limit});
                    parse_gateway_page(&self.post(self.endpoint("query"), body)?)
                }
                ApiMode::Atelier => {
                    // the Atelier API returns the whole result set in one response
                    if offset > 0 {
                        return Ok(Page::default());
                    }
                    let body = json!({"query": sql, "parameters": params});
                    parse_atelier_page(&self.post(self.endpoint("query"), body)?)
                }
            }
        }

        /// Run an INSERT / UPDATE / DELETE.
        fn exec(&self, sql: &str, params: &[JsonValue]) -> FdwResult {
            let (url, body) = match self.mode {
                ApiMode::Gateway => (self.endpoint("exec"), json!({"sql": sql, "params": params})),
                ApiMode::Atelier => (
                    self.endpoint("query"),
                    json!({"query": sql, "parameters": params}),
                ),
            };
            self.post(url, body)?;
            stats::inc_stats(FDW_NAME, stats::Metric::RowsOut, 1);
            Ok(())
        }

        // -------------------------------------------------------------------
        // Scan
        // -------------------------------------------------------------------

        fn source(ctx: &Context) -> Result<Source, FdwError> {
            let opts = ctx.get_options(&OptionsType::Table);
            if let Some(table) = opts.get("table") {
                Ok(Source::Table(validate_table_name(&table)?))
            } else if let Some(query) = opts.get("query") {
                Ok(Source::Query(validate_query(&query)?))
            } else {
                Err("foreign table option 'table' or 'query' is required".to_string())
            }
        }

        fn start_scan(&mut self, ctx: &Context) -> FdwResult {
            let source = Self::source(ctx)?;
            let table_opts = ctx.get_options(&OptionsType::Table);
            let limit_pushdown = table_opts
                .get("limit_pushdown")
                .map(|v| v == "true")
                .unwrap_or(self.limit_pushdown);

            let columns = ctx.get_columns();
            let names: Vec<String> = columns.iter().map(|c| c.name()).collect();
            let wants_attrs = names.iter().any(|n| n == "_attrs");
            let remote_cols = if wants_attrs {
                None
            } else {
                Some(names.clone())
            };

            let quals: Vec<QualSpec> = ctx
                .get_quals()
                .iter()
                .map(|q| QualSpec {
                    field: q.field(),
                    operator: q.operator(),
                    value: match q.value() {
                        Value::Cell(c) => cell_to_param(&c, self.mode).map(QualValue::Single),
                        Value::Array(cells) => cells
                            .iter()
                            .map(|c| cell_to_param(c, self.mode))
                            .collect::<Option<Vec<_>>>()
                            .map(QualValue::List),
                    },
                    use_or: q.use_or(),
                })
                .collect();

            let sorts: Vec<SortSpec> = ctx
                .get_sorts()
                .iter()
                .map(|s| {
                    let field = s.field();
                    let is_text = columns
                        .iter()
                        .find(|c| c.name() == field)
                        .map(|c| !matches!(
                            c.type_oid(),
                            TypeOid::Bool | TypeOid::I8 | TypeOid::I16 | TypeOid::I32 | TypeOid::I64
                                | TypeOid::F32 | TypeOid::F64 | TypeOid::Numeric | TypeOid::Date
                                | TypeOid::Timestamp | TypeOid::Timestamptz
                        ))
                        .unwrap_or(true);
                    SortSpec { field, reversed: s.reversed(), nulls_first: s.nulls_first(), is_text }
                })
                .collect();

            let limit = ctx.get_limit().map(|l| (l.count(), l.offset()));

            let plan = build_select(&SelectRequest {
                source: &source,
                columns: remote_cols,
                quals: &quals,
                sorts: &sorts,
                limit,
                limit_pushdown,
            });
            if self.debug {
                utils::report_info(&format!(
                    "iris_fdw: {} params={:?} max_rows={:?}",
                    plan.sql, plan.params, plan.max_rows
                ));
            }

            self.plan = Some(plan);
            self.fetched = 0;
            self.fetch_next_page()?;
            self.col_map = map_columns(&names, &self.page.columns)?;
            Ok(())
        }

        fn fetch_next_page(&mut self) -> FdwResult {
            let plan = self.plan.as_ref().ok_or("scan not started")?;
            let mut size = self.batch_size;
            if self.fetched == 0 {
                if let Some(hint) = plan.first_page_hint {
                    size = size.min(hint);
                }
            }
            if let Some(max) = plan.max_rows {
                size = size.min(max.saturating_sub(self.fetched));
            }
            self.page = if size == 0 {
                Page { columns: self.page.columns.clone(), ..Default::default() }
            } else {
                let mut page = self.query_page(&plan.sql, &plan.params, self.fetched, size)?;
                if page.columns.is_empty() {
                    page.columns = self.page.columns.clone();
                }
                page
            };
            self.page_idx = 0;
            self.fetched += self.page.rows.len();
            stats::inc_stats(FDW_NAME, stats::Metric::RowsIn, self.page.rows.len() as i64);
            Ok(())
        }

        fn has_more_rows(&self) -> bool {
            let under_max = self
                .plan
                .as_ref()
                .and_then(|p| p.max_rows)
                .map(|max| self.fetched < max)
                .unwrap_or(true);
            self.page.has_more && under_max
        }
    }

    // -----------------------------------------------------------------------
    // Cell conversions (need host types)
    // -----------------------------------------------------------------------

    fn cell_to_param(cell: &Cell, mode: ApiMode) -> Option<Param> {
        Some(match cell {
            Cell::Bool(b) => Param::Bool(*b),
            Cell::I8(n) => Param::Int(*n as i64),
            Cell::I16(n) => Param::Int(*n as i64),
            Cell::I32(n) => Param::Int(*n as i64),
            Cell::I64(n) => Param::Int(*n),
            Cell::F32(n) => Param::Float(*n as f64),
            Cell::F64(n) => Param::Float(*n),
            Cell::Numeric(n) => Param::Float(*n),
            Cell::String(s) => Param::Str(s.clone()),
            Cell::Date(secs) => match mode {
                // Atelier runs in logical mode: dates are $HOROLOG day numbers
                ApiMode::Atelier => Param::Int(secs.div_euclid(86_400) + HOROLOG_UNIX_OFFSET),
                ApiMode::Gateway => Param::Str(format_date_secs(*secs)),
            },
            Cell::Timestamp(us) | Cell::Timestamptz(us) => Param::Str(format_timestamp_micros(*us)),
            Cell::Json(s) => Param::Str(s.clone()),
            Cell::Uuid(s) => Param::Str(s.clone()),
            Cell::Other(_) => return None,
        })
    }

    fn opt_cell_to_param(cell: Option<&Cell>, mode: ApiMode) -> Result<Param, FdwError> {
        match cell {
            None => Ok(Param::Null),
            Some(c) => cell_to_param(c, mode).ok_or_else(|| "unsupported column type for IRIS".to_string()),
        }
    }

    fn json_to_cell(v: &JsonValue, col: &str, type_oid: &TypeOid) -> Result<Option<Cell>, FdwError> {
        if v.is_null() {
            return Ok(None);
        }
        let bad = || format!("cannot convert IRIS value {v} for column '{col}' to {type_oid:?}");
        let cell = match type_oid {
            TypeOid::Bool => Cell::Bool(json_to_bool(v).ok_or_else(bad)?),
            TypeOid::I8 => Cell::I8(json_to_int(v).ok_or_else(bad)?),
            TypeOid::I16 => Cell::I16(json_to_int(v).ok_or_else(bad)?),
            TypeOid::I32 => Cell::I32(json_to_int(v).ok_or_else(bad)?),
            TypeOid::I64 => Cell::I64(json_to_i64(v).ok_or_else(bad)?),
            TypeOid::F32 => Cell::F32(json_to_f64(v).ok_or_else(bad)? as f32),
            TypeOid::F64 => Cell::F64(json_to_f64(v).ok_or_else(bad)?),
            TypeOid::Numeric => Cell::Numeric(json_to_f64(v).ok_or_else(bad)?),
            TypeOid::String => Cell::String(json_to_string(v)),
            TypeOid::Date => {
                let days = parse_iris_date_days(v).ok_or_else(bad)?;
                Cell::Date(days * 86_400)
            }
            TypeOid::Timestamp => Cell::Timestamp(parse_iris_timestamp_micros(v).ok_or_else(bad)?),
            TypeOid::Timestamptz => Cell::Timestamptz(parse_iris_timestamp_micros(v).ok_or_else(bad)?),
            TypeOid::Json => Cell::Json(json_to_json_text(v)),
            TypeOid::Uuid => Cell::Uuid(json_to_string(v)),
            TypeOid::Other(t) => {
                return Err(format!(
                    "column '{col}' has unsupported type '{t}': declare it as text, numeric, date, timestamp or jsonb"
                ));
            }
        };
        Ok(Some(cell))
    }

    fn row_params(row: &Row, mode: ApiMode) -> Result<Vec<(String, Param)>, FdwError> {
        row.cols()
            .into_iter()
            .zip(row.cells())
            .filter(|(col, _)| col != "_attrs")
            .map(|(col, cell)| Ok((col, opt_cell_to_param(cell.as_ref(), mode)?)))
            .collect()
    }

    // -----------------------------------------------------------------------
    // FDW routines
    // -----------------------------------------------------------------------

    impl Guest for IrisFdw {
        fn host_version_requirement() -> String {
            // version of the Wasm FDW host inside the Supabase Wrappers extension
            "^0.1.0".to_string()
        }

        fn init(ctx: &Context) -> FdwResult {
            Self::init_instance();
            let this = Self::this_mut();

            let opts = ctx.get_options(&OptionsType::Server);
            this.base_url = opts.require("api_url")?.trim_end_matches('/').to_string();
            this.mode = match opts.require_or("api_mode", "gateway").to_ascii_lowercase().as_str() {
                "gateway" => ApiMode::Gateway,
                "atelier" => ApiMode::Atelier,
                other => return Err(format!("invalid api_mode '{other}': use 'gateway' or 'atelier'")),
            };
            this.gateway_path = opts.require_or("gateway_path", "/iris-fdw/v1").trim_end_matches('/').to_string();
            this.namespace = opts.require_or("namespace", "USER");
            this.batch_size = opts
                .require_or("batch_size", "1000")
                .parse::<usize>()
                .map_err(|_| "batch_size must be a positive integer".to_string())?
                .max(1);
            this.limit_pushdown = opts.require_or("limit_pushdown", "false") == "true";
            this.debug = opts.require_or("debug", "false") == "true";

            let username = opts.require("username")?;
            let password = match opts.get("password") {
                Some(p) => p,
                None => {
                    let secret_id = opts
                        .get("password_id")
                        .ok_or("server option 'password' or 'password_id' (Vault secret id) is required")?;
                    utils::get_vault_secret(&secret_id)
                        .ok_or(format!("Vault secret '{secret_id}' not found"))?
                }
            };
            let token = base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));

            this.headers = vec![
                ("user-agent".to_owned(), "Supabase Wrappers IRIS FDW".to_owned()),
                ("content-type".to_owned(), "application/json".to_owned()),
                ("accept".to_owned(), "application/json".to_owned()),
                ("authorization".to_owned(), format!("Basic {token}")),
            ];

            stats::inc_stats(FDW_NAME, stats::Metric::CreateTimes, 1);
            Ok(())
        }

        fn begin_scan(ctx: &Context) -> FdwResult {
            Self::this_mut().start_scan(ctx)
        }

        fn iter_scan(ctx: &Context, row: &Row) -> Result<Option<u32>, FdwError> {
            let this = Self::this_mut();

            if this.page_idx >= this.page.rows.len() {
                if !this.has_more_rows() {
                    return Ok(None);
                }
                this.fetch_next_page()?;
                if this.page.rows.is_empty() {
                    return Ok(None);
                }
            }

            let src = &this.page.rows[this.page_idx];
            for (i, col) in ctx.get_columns().iter().enumerate() {
                let cell = match this.col_map.get(i).copied().flatten() {
                    Some(idx) => {
                        let v = src.get(idx).unwrap_or(&JsonValue::Null);
                        json_to_cell(v, &col.name(), &col.type_oid())?
                    }
                    None => Some(Cell::Json(row_to_attrs(&this.page.columns, src))),
                };
                row.push(cell.as_ref());
            }
            this.page_idx += 1;
            Ok(Some(0))
        }

        fn re_scan(ctx: &Context) -> FdwResult {
            Self::this_mut().start_scan(ctx)
        }

        fn end_scan(_ctx: &Context) -> FdwResult {
            let this = Self::this_mut();
            this.plan = None;
            this.page = Page::default();
            this.page_idx = 0;
            this.fetched = 0;
            Ok(())
        }

        fn begin_modify(ctx: &Context) -> FdwResult {
            let this = Self::this_mut();
            let opts = ctx.get_options(&OptionsType::Table);
            this.table = validate_table_name(
                &opts
                    .get("table")
                    .ok_or("foreign tables defined with 'query' are read-only; use the 'table' option")?,
            )?;
            this.rowid_col = opts.require("rowid_column")?;
            Ok(())
        }

        fn insert(_ctx: &Context, row: &Row) -> FdwResult {
            let this = Self::this_mut();
            let cols: Vec<_> = row_params(row, this.mode)?
                .into_iter()
                .filter(|(_, p)| *p != Param::Null)
                .collect();
            let (sql, params) = build_insert(&this.table, &cols);
            this.exec(&sql, &params)
        }

        fn update(_ctx: &Context, rowid: Cell, row: &Row) -> FdwResult {
            let this = Self::this_mut();
            let rowid = opt_cell_to_param(Some(&rowid), this.mode)?;
            match build_update(&this.table, &this.rowid_col, &rowid, &row_params(row, this.mode)?) {
                Some((sql, params)) => this.exec(&sql, &params),
                None => Ok(()),
            }
        }

        fn delete(_ctx: &Context, rowid: Cell) -> FdwResult {
            let this = Self::this_mut();
            let rowid = opt_cell_to_param(Some(&rowid), this.mode)?;
            let (sql, params) = build_delete(&this.table, &this.rowid_col, &rowid);
            this.exec(&sql, &params)
        }

        fn end_modify(_ctx: &Context) -> FdwResult {
            Ok(())
        }

        fn import_foreign_schema(
            _ctx: &Context,
            stmt: ImportForeignSchemaStmt,
        ) -> Result<Vec<String>, FdwError> {
            let this = Self::this_mut();
            let params = vec![json!(stmt.remote_schema)];

            let mut columns = Vec::new();
            let mut offset = 0;
            loop {
                let page = this.query_page(IMPORT_COLUMNS_SQL, &params, offset, 10_000)?;
                offset += page.rows.len();
                columns.extend(import_columns_from_page(&page)?);
                if !page.has_more || page.rows.is_empty() {
                    break;
                }
            }
            if columns.is_empty() {
                return Err(format!(
                    "no tables found in IRIS schema '{}' (or no SQL privileges on them)",
                    stmt.remote_schema
                ));
            }

            let list: Vec<String> = stmt.table_list.iter().map(|t| t.to_lowercase()).collect();
            let list_type = stmt.list_type;
            let include = |t: &str| {
                let t = t.to_lowercase();
                match list_type {
                    ImportSchemaType::All => true,
                    ImportSchemaType::LimitTo => list.contains(&t),
                    ImportSchemaType::Except => !list.contains(&t),
                }
            };
            Ok(build_import_statements(
                &stmt.server_name,
                &stmt.local_schema,
                &stmt.remote_schema,
                &columns,
                include,
            ))
        }
    }

    crate::bindings::export!(IrisFdw with_types_in crate::bindings);
}
