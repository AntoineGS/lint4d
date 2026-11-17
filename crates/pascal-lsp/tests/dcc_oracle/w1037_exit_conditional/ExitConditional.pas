program ExitConditional;

procedure Demo(Stop: Boolean);
var
  I: Integer;
begin
  for I := 1 to 3 do
    if Stop then
      Exit;
  WriteLn(I);
end;

begin
  Demo(False);
end.
