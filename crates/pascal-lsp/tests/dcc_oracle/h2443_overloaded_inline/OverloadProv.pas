unit OverloadProv;
interface
function InlineValue(X: Integer): Integer; overload; inline;
function InlineValue(X: string): Integer; overload; inline;
implementation
uses OverloadHelper;
function InlineValue(X: Integer): Integer;
begin
  Result := HelperValue + X;
end;
function InlineValue(X: string): Integer;
begin
  Result := Length(X);
end;
end.
