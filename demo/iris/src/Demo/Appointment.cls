Class Demo.Appointment Extends %Persistent
{

Property Patient As Demo.Patient [ Required ];

Property Doctor As %String(MAXLEN = 100);

Property Specialty As %String(MAXLEN = 50);

Property StartsAt As %TimeStamp;

Property Status As %String(VALUELIST = ",scheduled,completed,cancelled");

Property Cost As %Numeric(SCALE = 2);

Property Paid As %Boolean;

Index PatientIdx On Patient;

}
