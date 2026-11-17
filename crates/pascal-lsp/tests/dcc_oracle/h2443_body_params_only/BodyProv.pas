unit BodyProv;
interface
function DoubleValue(X: Integer): Integer; inline;
implementation
function DoubleValue(X: Integer): Integer;
begin
  Result := X * 2;
end;
end.
