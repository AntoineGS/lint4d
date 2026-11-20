program SwitchW1036;

{$WARN USE_BEFORE_DEF OFF}
procedure Hidden;
var
  Value: Integer;
begin
  WriteLn(Value);
end;

{$WARN USE_BEFORE_DEF ON}
procedure Visible;
var
  Value: Integer;
begin
  WriteLn(Value);
end;

begin
  Hidden;
  Visible;
end.
