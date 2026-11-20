program NonconstantBounds;

procedure Demo(Limit: Integer; Enabled: Boolean);
const
  TypedLimit: Integer = 0;
var
  I: Integer;
begin
  for I := 1 to Limit do
    WriteLn(I);
  for I := 1 to TypedLimit do
    WriteLn(I);
  while Enabled do
    Enabled := False;
end;

begin
  Demo(3, True);
end.
