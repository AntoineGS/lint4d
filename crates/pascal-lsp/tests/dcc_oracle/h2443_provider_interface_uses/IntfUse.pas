unit IntfUse;
interface
implementation
uses IntfProv;
procedure P;
var I: Integer;
begin
  I := UsesInterfaceUnit;
end;
end.
