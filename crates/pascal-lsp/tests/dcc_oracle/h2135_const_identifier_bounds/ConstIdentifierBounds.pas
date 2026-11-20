program ConstIdentifierBounds;

uses
  ConstBounds;

procedure Demo;
const
  LocalN = 0;
  Disabled = False;
var
  I: Integer;
begin
  for I := 1 to N do
    WriteLn(I);
  for I := 1 to LocalN do
    WriteLn(I);
  while Disabled do
    WriteLn(I);
end;

begin
  Demo;
end.
