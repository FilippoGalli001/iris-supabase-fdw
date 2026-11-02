/// HTTP/JSON SQL gateway used by the Supabase Wasm foreign data wrapper (iris_fdw).
/// <p>
/// Supabase Wasm FDWs cannot open TCP connections to a database: they can only call
/// HTTP(S) JSON APIs. This class exposes a minimal, stateless SQL-over-HTTP API:
/// <ul>
/// <li><b>GET  /ping</b>  - health check and version info</li>
/// <li><b>POST /query</b> - run a parameterized SELECT, returns one page of rows</li>
/// <li><b>POST /exec</b>  - run a parameterized INSERT / UPDATE / DELETE</li>
/// </ul>
/// Every statement runs as the authenticated IRIS user through Dynamic SQL, so the
/// SQL privileges granted to that user (GRANT SELECT/INSERT/... ) are enforced by IRIS.
/// Values are always bound as <b>?</b> parameters: they are never concatenated into SQL.
Class FDW.Gateway.REST Extends %CSP.REST
{

Parameter VERSION = "0.1.0";

Parameter CHARSET = "utf-8";

Parameter CONTENTTYPE = "application/json";

/// Each FDW request is independent: never open a CSP session (saves license slots).
Parameter UseSession As Integer = 0;

Parameter HandleCorsRequest = 1;

/// Default and maximum number of rows returned by one /query call.
Parameter DEFAULTPAGESIZE = 1000;

Parameter MAXPAGESIZE = 50000;

/// Maximum number of characters read from a stream column.
Parameter MAXSTREAMLEN = 1000000;

XData UrlMap [ XMLNamespace = "http://www.intersystems.com/urlmap" ]
{
<Routes>
<Route Url="/ping" Method="GET" Call="Ping"/>
<Route Url="/query" Method="POST" Call="Query"/>
<Route Url="/exec" Method="POST" Call="Exec"/>
</Routes>
}

/// GET /ping
ClassMethod Ping() As %Status
{
    set resp = {
        "service": "iris-fdw-gateway",
        "version": (..#VERSION),
        "namespace": ($namespace),
        "user": ($username),
        "iris": ($zversion)
    }
    do resp.%Set("readOnly", ..IsReadOnly(), "boolean")
    write resp.%ToJSON()
    quit $$$OK
}

/// POST /query  body: {"sql": "...", "params": [...], "offset": 0, "limit": 1000}
/// <br>Response: {"columns": [{"name","type","odbcType"}], "rows": [[...], ...], "hasMore": bool}
ClassMethod Query() As %Status
{
    try {
        set body = ..ReadBody()
        set stmt = ..Prepare(body.sql)
        set type = stmt.%Metadata.statementType
        if type '= 1 {
            $$$ThrowStatus($$$ERROR($$$GeneralError, "/query only accepts SELECT statements"))
        }
        set offset = +body.offset
        set limit = $select(body.limit = "": ..#DEFAULTPAGESIZE, 1: +body.limit)
        if (limit < 1) || (limit > ..#MAXPAGESIZE) set limit = ..#MAXPAGESIZE

        do ..BindParams(body.params, .args)
        set rs = stmt.%Execute(args...)
        if rs.%SQLCODE < 0 {
            do ..SQLError(rs.%SQLCODE, rs.%Message)
            quit
        }

        // column metadata
        set cols = stmt.%Metadata.columns
        set ncols = cols.Count()
        set columns = []
        for i = 1:1:ncols {
            set col = cols.GetAt(i)
            set ctype(i) = col.clientType
            do columns.%Push({"name": (col.label), "type": (..ClientTypeName(col.clientType)), "odbcType": (col.ODBCType)})
        }

        // skip the rows of the previous pages (stateless paging)
        set eof = 0
        for i = 1:1:offset {
            if 'rs.%Next(.sc) {
                $$$ThrowOnError(sc)
                set eof = 1
                quit
            }
        }

        // build the whole page before writing anything, so that an error raised
        // while reading rows still produces a clean JSON error response
        set rows = []
        while ('eof) && (rows.%Size() < limit) {
            if 'rs.%Next(.sc) {
                $$$ThrowOnError(sc)
                set eof = 1
                quit
            }
            set row = []
            for i = 1:1:ncols {
                do ..PushValue(row, rs.%GetData(i), ctype(i))
            }
            do rows.%Push(row)
        }
        set hasMore = 0
        if 'eof {
            set hasMore = rs.%Next(.sc)
            $$$ThrowOnError(sc)
        }
        if rs.%SQLCODE < 0 {
            // runtime SQL errors (e.g. division by zero) end the iteration
            do ..SQLError(rs.%SQLCODE, rs.%Message)
            quit
        }
        set resp = {"columns": (columns), "rows": (rows), "rowCount": (rows.%Size())}
        do resp.%Set("hasMore", hasMore, "boolean")
        do resp.%ToJSON()
    } catch ex {
        do ..ExceptionError(ex)
    }
    quit $$$OK
}

/// POST /exec  body: {"sql": "INSERT|UPDATE|DELETE ...", "params": [...]}
/// <br>Response: {"rowCount": n, "rowId": "..."}
ClassMethod Exec() As %Status
{
    try {
        if ..IsReadOnly() {
            do ..RenderError(403, "gateway is configured as read-only (^FDW.Gateway(""readOnly"")=1)")
            quit
        }
        set body = ..ReadBody()
        set stmt = ..Prepare(body.sql)
        set type = stmt.%Metadata.statementType
        // 2=INSERT, 3=UPDATE, 4=DELETE, 25=INSERT OR UPDATE
        if '$listfind($listbuild(2, 3, 4, 25), type) {
            $$$ThrowStatus($$$ERROR($$$GeneralError, "/exec only accepts INSERT, UPDATE or DELETE statements"))
        }
        do ..BindParams(body.params, .args)
        set rs = stmt.%Execute(args...)
        if rs.%SQLCODE < 0 {
            do ..SQLError(rs.%SQLCODE, rs.%Message)
            quit
        }
        set resp = {"rowCount": (+rs.%ROWCOUNT), "rowId": (rs.%ROWID)}
        write resp.%ToJSON()
    } catch ex {
        do ..ExceptionError(ex)
    }
    quit $$$OK
}

/// Parse the JSON request body
ClassMethod ReadBody() As %DynamicObject
{
    if '$isobject(%request.Content) {
        $$$ThrowStatus($$$ERROR($$$GeneralError, "missing JSON request body"))
    }
    set body = ##class(%DynamicObject).%FromJSON(%request.Content)
    if body.sql = "" {
        $$$ThrowStatus($$$ERROR($$$GeneralError, "missing 'sql' property"))
    }
    quit body
}

/// Prepare a statement in ODBC select mode: dates/timestamps are exchanged as ISO-like strings.
ClassMethod Prepare(sql As %String) As %SQL.Statement
{
    set stmt = ##class(%SQL.Statement).%New(1)
    $$$ThrowOnError(stmt.%Prepare(sql))
    quit stmt
}

/// Convert the JSON "params" array into a multidimensional array usable as args...
ClassMethod BindParams(params As %DynamicArray, Output args)
{
    kill args
    set args = 0
    quit:'$isobject(params)
    set size = params.%Size()
    for i = 0:1:size - 1 {
        set jtype = params.%GetTypeOf(i)
        set val = params.%Get(i)
        if jtype = "null" {
            set val = ""
        } elseif jtype = "boolean" {
            set val = ''val
        } elseif (jtype = "string") && (val = "") {
            // in IRIS SQL "" is NULL, $c(0) is the empty string
            set val = $char(0)
        } elseif (jtype = "object") || (jtype = "array") {
            set val = val.%ToJSON()
        }
        set args($increment(args)) = val
    }
}

/// Append one SQL value to a JSON row with the correct JSON type
ClassMethod PushValue(row As %DynamicArray, val, clientType As %Integer)
{
    if val = "" {
        do row.%Push("", "null")
    } elseif val = $char(0) {
        do row.%Push("")
    } elseif $listfind($listbuild(3, 5, 14, 15, 18), clientType) {
        // DOUBLE, INTEGER, NUMERIC, CURRENCY, BIGINT
        do row.%Push(..CanonicalNumber(val), "number")
    } elseif clientType = 16 {
        do row.%Push(''val, "boolean")
    } elseif (clientType = 12) || (clientType = 13) {
        do row.%Push(..ReadStream(val, clientType = 12))
    } else {
        do row.%Push(val)
    }
}

/// IRIS canonical numbers may look like ".5" or "-.5", which are not valid JSON
ClassMethod CanonicalNumber(val) As %String
{
    set val = +val
    if $extract(val) = "." quit "0"_val
    if $extract(val, 1, 2) = "-." quit "-0"_$extract(val, 2, *)
    quit val
}

/// Read a stream column (binary streams are hex encoded)
ClassMethod ReadStream(oid, binary As %Boolean = 0) As %String
{
    set stream = ##class(%Stream.Object).%Open(oid)
    quit:'$isobject(stream) ""
    set data = stream.Read(..#MAXSTREAMLEN)
    if binary {
        set hex = ""
        for i = 1:1:$length(data) set hex = hex _ $translate($justify($zhex($ascii(data, i)), 2), " ", "0")
        quit hex
    }
    quit data
}

ClassMethod ClientTypeName(clientType As %Integer) As %String
{
    quit $case(clientType,
        1: "binary", 2: "date", 3: "double", 5: "integer", 7: "longvarchar", 8: "time",
        9: "timestamp", 10: "varchar", 12: "binarystream", 13: "characterstream",
        14: "numeric", 15: "currency", 16: "boolean", 18: "bigint", 20: "timestamp",
        : "varchar")
}

ClassMethod IsReadOnly() As %Boolean
{
    quit ''$get(^FDW.Gateway("readOnly"), 0)
}

ClassMethod SQLError(sqlcode As %Integer, message As %String)
{
    do ..RenderError(400, message, sqlcode)
}

ClassMethod ExceptionError(ex As %Exception.AbstractException)
{
    if ex.%IsA("%Exception.SQL") {
        do ..RenderError(400, ex.Data, ex.Code)
    } else {
        set text = $system.Status.GetOneStatusText(ex.AsStatus())
        if text [ "SQLCODE: " {
            // e.g. "SQLCODE: -30 Message:  Table 'X' not found" from %Prepare
            set sqlcode = +$piece($piece(text, "SQLCODE: ", 2), " ")
            set msg = $zstrip($piece(text, "Message:", 2, *), "<>W")
            do ..RenderError(400, msg, sqlcode)
        } else {
            do ..RenderError(400, text)
        }
    }
}

ClassMethod RenderError(httpStatus As %Integer, message As %String, sqlcode As %Integer = "")
{
    set %response.Status = httpStatus _ " " _ $case(httpStatus, 400: "Bad Request", 403: "Forbidden", : "Error")
    set err = {"message": (message)}
    if sqlcode '= "" do err.%Set("sqlcode", sqlcode, "number")
    write {"error": (err)}.%ToJSON()
}

}
