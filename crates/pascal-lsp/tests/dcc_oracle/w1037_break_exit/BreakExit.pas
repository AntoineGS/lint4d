program BreakExit;

procedure Demo(Last: Integer; Stop: Boolean);
var
  I: Integer;
begin
  for I := 1 to Last do
    if Stop then
      Break;
  WriteLn(I);
end;

begin
  Demo(5, True);
end.
