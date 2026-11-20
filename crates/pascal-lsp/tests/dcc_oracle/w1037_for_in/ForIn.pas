program ForIn;

type
  TNumbers = set of 1..2;

procedure Demo(Values: TNumbers);
var
  I: Integer;
begin
  for I in Values do
    WriteLn(I);
  WriteLn(I);
end;

begin
  Demo([1, 2]);
end.
