program LastAssignmentUnused;

procedure Demo;
var
  Value: Integer;
begin
  Value := 1;
  if Value = 1 then
    WriteLn('used');
  Value := 2;
end;

begin
  Demo;
end.
