program SwitchH2164;

{$HINTS OFF}
procedure Hidden;
var
  NeverUsed: Integer;
begin
end;

{$HINTS ON}
procedure Visible;
var
  AlsoUnused: Integer;
begin
end;

begin
  Hidden;
  Visible;
end.
