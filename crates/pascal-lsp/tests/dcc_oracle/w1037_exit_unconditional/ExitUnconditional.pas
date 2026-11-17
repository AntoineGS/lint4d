program ExitUnconditional;

procedure Demo;
var
  I: Integer;
begin
  for I := 1 to 3 do
  begin
    WriteLn(I);
    Exit;
  end;
  WriteLn(I);
end;

begin
  Demo;
end.
