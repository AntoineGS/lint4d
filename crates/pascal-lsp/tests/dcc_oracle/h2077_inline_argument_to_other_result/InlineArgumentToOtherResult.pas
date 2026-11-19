unit InlineArgumentToOtherResult;

interface

function G(Value: Integer): Integer; inline;

implementation

function G(Value: Integer): Integer;
begin
  Result := Value + 1;
end;

procedure P;
var
  I, J: Integer;
begin
  I := 1;
  J := G(I);
  Writeln(J);
end;

end.
