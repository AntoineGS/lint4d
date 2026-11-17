unit SameUnit;
interface
function UsesHelper: Integer; inline;
implementation
uses SameHelper;
function UsesHelper: Integer;
begin
  Result := HelperValue;
end;
procedure P;
var I: Integer;
begin
  I := UsesHelper;
end;
end.
