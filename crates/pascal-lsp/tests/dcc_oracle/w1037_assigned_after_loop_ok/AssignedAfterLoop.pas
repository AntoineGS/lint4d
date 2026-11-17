program AssignedAfterLoop;

procedure Demo(Last: Integer);
var
  I: Integer;
begin
  for I := 2 to Last do
    WriteLn(I);
  I := 99;
  WriteLn(I);
end;

begin
  Demo(5);
end.
