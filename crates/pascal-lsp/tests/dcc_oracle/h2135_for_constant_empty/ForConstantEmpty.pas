program ForConstantEmpty;

procedure Demo;
var
  I: Integer;
  C: Char;
begin
  for I := 1 to 0 do
    WriteLn(I);
  for I := 1 + 1 to 1 do
    WriteLn(I);
  for C := 'b' to 'a' do
    WriteLn(C);
end;

begin
  Demo;
end.
