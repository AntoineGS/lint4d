program SwitchH2135;

{$HINTS OFF}
procedure Hidden;
var
  I: Integer;
begin
  for I := 1 to 0 do
    WriteLn(I);
end;

{$HINTS ON}
procedure Visible;
var
  I: Integer;
begin
  for I := 1 to 0 do
    WriteLn(I);
end;

begin
  Hidden;
  Visible;
end.
