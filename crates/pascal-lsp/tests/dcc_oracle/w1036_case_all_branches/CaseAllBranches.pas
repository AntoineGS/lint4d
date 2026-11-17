program CaseAllBranches;
procedure Demo(Choice: Integer);
var
  Value: Integer;
begin
  case Choice of
    0: Value := 1;
  else
    Value := 2;
  end;
  WriteLn(Value);
end;
begin
  Demo(0);
end.
