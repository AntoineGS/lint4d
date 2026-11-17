unit InlProv;
interface
function UsesImplUnit: Integer; inline;
function UsesNothing(X: Integer): Integer; inline;
function UsesIntfUnitType: Integer; inline;
implementation
uses InlHelper;
function UsesImplUnit: Integer;
begin
  Result := HelperValue + 1;
end;
function UsesNothing(X: Integer): Integer;
begin
  Result := X * 2;
end;
function UsesIntfUnitType: Integer;
var R: TRec;
begin
  R.A := 3;
  Result := R.A;
end;
end.
