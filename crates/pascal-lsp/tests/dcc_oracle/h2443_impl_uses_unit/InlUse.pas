unit InlUse;
interface
implementation
uses InlProv;
procedure P;
var I: Integer;
begin
  I := UsesImplUnit;
  I := UsesNothing(I);
  I := UsesIntfUnitType;
  I := UsesImplUnit;
  if I = 0 then;
end;
end.
