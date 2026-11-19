program NestedResultCapture;
function Answer: Integer;
  procedure SetResult;
  begin
    Result := 42;
  end;
begin
  SetResult;
end;
begin
  WriteLn(Answer);
end.
