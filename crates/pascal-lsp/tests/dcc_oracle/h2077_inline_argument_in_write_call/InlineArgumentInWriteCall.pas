unit InlineArgumentInWriteCall;

interface

function F: Integer; inline;
function G(Value: Integer): Integer; inline;

implementation

function F: Integer;
begin
  Result := 1;
end;

function G(Value: Integer): Integer;
begin
  Result := Value + 1;
end;

procedure P;
var
  I: Integer;
begin
  I := F;
  Writeln(G(I));
end;

end.
