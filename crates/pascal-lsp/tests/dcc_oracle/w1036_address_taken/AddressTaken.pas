program AddressTaken;
procedure Demo;
var
  Value: Integer;
  Address: Pointer;
begin
  Address := @Value;
  if Address <> nil then
    WriteLn(Value);
end;
begin
  Demo;
end.
