program IfOnlyBranch;
procedure Demo(Flag: Boolean);
var
  Value: Integer;
begin
  if Flag then
    Value := 1;
  WriteLn(Value);
  Value := 2;
  WriteLn(Value);
end;
begin
  Demo(True);
end.
