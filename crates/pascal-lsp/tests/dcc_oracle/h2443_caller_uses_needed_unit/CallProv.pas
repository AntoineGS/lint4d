unit CallProv;
interface
function UsesHelper: Integer; inline;
implementation
uses CallHelper;
function UsesHelper: Integer;
begin
  Result := HelperValue;
end;
end.
