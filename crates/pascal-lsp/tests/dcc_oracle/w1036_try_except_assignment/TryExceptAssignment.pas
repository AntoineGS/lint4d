program TryExceptAssignment;
procedure Demo;
var
  Value: Integer;
begin
  try
    Value := 1;
  except
  end;
  WriteLn(Value);
  Value := 2;
  WriteLn(Value);
end;
begin
  Demo;
end.
