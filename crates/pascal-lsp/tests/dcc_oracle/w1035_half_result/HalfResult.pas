program HalfResult;
function Answer(Flag: Boolean): Integer;
begin
  if Flag then
    Result := 1;
end;
begin
  WriteLn(Answer(True));
end.
