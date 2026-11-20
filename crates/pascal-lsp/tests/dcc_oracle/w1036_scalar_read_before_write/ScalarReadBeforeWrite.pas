program ScalarReadBeforeWrite;
procedure Demo;
var
  Value: Integer;
begin
  WriteLn(Value);
  WriteLn(Value);
  Value := 1;
  WriteLn(Value);
end;
begin
  Demo;
end.
