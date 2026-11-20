program SwitchH2077;

{$HINTS OFF}
procedure Hidden;
var
  Value: Integer;
begin
  Value := 1;
end;

{$HINTS ON}
procedure Visible;
var
  Value: Integer;
begin
  Value := 1;
end;

begin
  Hidden;
  Visible;
end.
