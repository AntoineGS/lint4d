program OverwrittenInConstantLoop;

procedure Demo;
var
  I, Value: Integer;
begin
  Value := 1;
  for I := 1 to 2 do
    Value := I;
  WriteLn(Value);
end;

begin
  Demo;
end.
