program OutArgInitializes;
procedure Initialize(out Value: Integer);
begin
  Value := 1;
end;
procedure Demo;
var
  Value: Integer;
begin
  Initialize(Value);
  WriteLn(Value);
end;
begin
  Demo;
end.
