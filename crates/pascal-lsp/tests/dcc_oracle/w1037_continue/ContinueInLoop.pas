program ContinueInLoop;

procedure Demo;
var
  I: Integer;
begin
  for I := 1 to 3 do
    Continue;
  WriteLn(I);
end;

begin
  Demo;
end.
