unit RepeatProv;
interface
function UsesHelper: Integer; inline;
implementation
uses RepeatHelper;
function UsesHelper: Integer;
begin
  Result := HelperValue;
end;
end.
