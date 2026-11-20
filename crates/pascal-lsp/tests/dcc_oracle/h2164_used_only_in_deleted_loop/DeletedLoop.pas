program DeletedLoop;

procedure Demo;
var
  I, LoopOnly: Integer;
begin
  for I := 1 to 0 do
    if LoopOnly = 0 then
      WriteLn('unreachable');
end;

begin
  Demo;
end.
