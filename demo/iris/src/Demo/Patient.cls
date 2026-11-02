/// Demo clinical data that lives in InterSystems IRIS and is queried from Supabase.
Class Demo.Patient Extends %Persistent
{

Property Name As %String(MAXLEN = 100) [ Required ];

Property BirthDate As %Date;

Property City As %String(MAXLEN = 50);

Property Email As %String(MAXLEN = 100);

Property Active As %Boolean [ InitialExpression = 1 ];

Property RiskScore As %Numeric(SCALE = 2);

Property Notes As %Stream.GlobalCharacter;

/// Clinical profile embedding (age, risk, visits, cardiology share), used for
/// IRIS vector search: VECTOR_COSINE(a.Profile, b.Profile)
Property Profile As %Library.Vector(DATATYPE = "DOUBLE", LEN = 4);

Index CityIdx On City;

}
