unit RepeatedUse;
interface
implementation
uses RepeatProv;
procedure P;
var I: Integer;
begin
  I := UsesHelper;
  I := UsesHelper;
  I := UsesHelper;
end;
end.
