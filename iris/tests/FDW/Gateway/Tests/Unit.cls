/// Unit tests for the gateway value conversions.
/// Run with: zpm "iris-fdw-gateway test -only"
Class FDW.Gateway.Tests.Unit Extends %UnitTest.TestCase
{

Method TestCanonicalNumber()
{
    do $$$AssertEquals(##class(FDW.Gateway.REST).CanonicalNumber(".5"), "0.5")
    do $$$AssertEquals(##class(FDW.Gateway.REST).CanonicalNumber("-.25"), "-0.25")
    do $$$AssertEquals(##class(FDW.Gateway.REST).CanonicalNumber("012.50"), "12.5")
    do $$$AssertEquals(##class(FDW.Gateway.REST).CanonicalNumber("42"), "42")
}

Method TestBindParams()
{
    set params = [null, true, false, "", "text", 12.5, {"a": 1}]
    do ##class(FDW.Gateway.REST).BindParams(params, .args)
    do $$$AssertEquals(args, 7)
    do $$$AssertEquals(args(1), "", "JSON null -> SQL NULL")
    do $$$AssertEquals(args(2), 1, "true -> 1")
    do $$$AssertEquals(args(3), 0, "false -> 0")
    do $$$AssertEquals(args(4), $char(0), "empty string -> $c(0)")
    do $$$AssertEquals(args(5), "text")
    do $$$AssertEquals(args(6), 12.5)
    do $$$AssertEquals(args(7), "{""a"":1}", "objects are serialized")
}

Method TestPushValue()
{
    set row = []
    do ##class(FDW.Gateway.REST).PushValue(row, "", 10)
    do ##class(FDW.Gateway.REST).PushValue(row, $char(0), 10)
    do ##class(FDW.Gateway.REST).PushValue(row, ".5", 14)
    do ##class(FDW.Gateway.REST).PushValue(row, 1, 16)
    do ##class(FDW.Gateway.REST).PushValue(row, "2026-11-02", 2)
    do ##class(FDW.Gateway.REST).PushValue(row, "007", 10)
    do $$$AssertEquals(row.%ToJSON(), "[null,"""",0.5,true,""2026-11-02"",""007""]")
}

Method TestStatementTypes()
{
    // the gateway relies on these statement type codes
    for sql = "SELECT 1", "INSERT INTO FDW_Tests.Dummy (x) VALUES (1)", "UPDATE FDW_Tests.Dummy SET x = 1", "DELETE FROM FDW_Tests.Dummy" {
        set stmt = ##class(%SQL.Statement).%New()
        set sc = stmt.%Prepare(sql)
        set types($piece(sql, " ")) = $select($$$ISOK(sc): stmt.%Metadata.statementType, 1: "")
    }
    do $$$AssertEquals(types("SELECT"), 1)
    do $$$AssertEquals($get(types("INSERT")), 2)
    do $$$AssertEquals($get(types("UPDATE")), 3)
    do $$$AssertEquals($get(types("DELETE")), 4)
}

Method OnBeforeAllTests() As %Status
{
    do ##class(%SQL.Statement).%ExecDirect(, "CREATE TABLE FDW_Tests.Dummy (x INTEGER)")
    quit $$$OK
}

Method OnAfterAllTests() As %Status
{
    do ##class(%SQL.Statement).%ExecDirect(, "DROP TABLE FDW_Tests.Dummy")
    quit $$$OK
}

}
