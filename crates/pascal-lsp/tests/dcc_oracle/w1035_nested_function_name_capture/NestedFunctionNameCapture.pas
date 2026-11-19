program NestedFunctionNameCapture;
function Answer: Integer;
  procedure SetResult;
  begin
    Answer := 42;
  end;
begin
  SetResult;
end;
begin
  WriteLn(Answer);
end.
