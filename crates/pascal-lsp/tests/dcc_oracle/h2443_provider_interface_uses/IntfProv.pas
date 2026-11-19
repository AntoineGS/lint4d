unit IntfProv;
interface
uses IntfHelper;
function UsesInterfaceUnit: Integer; inline;
implementation
function UsesInterfaceUnit: Integer;
begin
  Result := HelperValue;
end;
end.
