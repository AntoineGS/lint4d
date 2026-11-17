unit E2003BuiltinsOk;
interface
implementation
procedure P;
var I: Integer; P: Pointer;
begin
  I := Length('a') + SizeOf(Integer) + High(Byte);
  Inc(I);
  if Assigned(Pointer(nil)) then Inc(I);
  P := TypeInfo(Integer);
  if P = nil then Inc(I);
  while I < 0 do Break;
  Writeln(I);
  Exit;
end;
end.
