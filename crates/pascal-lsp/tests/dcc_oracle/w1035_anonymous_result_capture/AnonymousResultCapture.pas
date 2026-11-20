program AnonymousResultCapture;
type
  TProc = reference to procedure;
function Answer: Integer;
var
  SetResult: TProc;
begin
  SetResult := procedure
    begin
      Result := 42;
    end;
  SetResult();
end;
begin
  WriteLn(Answer);
end.
