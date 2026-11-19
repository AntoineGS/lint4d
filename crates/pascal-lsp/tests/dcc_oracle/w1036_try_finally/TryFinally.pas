program TryFinally;
procedure Demo(Flag: Boolean);
var
  Value: Integer;
begin
  try
    if Flag then
      Value := 1;
  finally
    Value := 2;
  end;
  WriteLn(Value);
end;
begin
  Demo(True);
end.
