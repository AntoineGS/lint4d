unit E2003UnitQualified;
interface
implementation
uses Provider;
procedure P;
var X: Integer;
begin
  System.Writeln('a');
  X := System.Zz2;
  X := Provider.ProvidedName;
  X := Provider.Zz3;
end;
end.
