unit E2003ViaUses;
interface
implementation
uses Provider;
procedure P;
var X: Integer;
begin
  X := ProvidedName;
  X := NotProvided;
end;
end.
