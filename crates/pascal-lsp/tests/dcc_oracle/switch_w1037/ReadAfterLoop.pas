program SwitchW1037;

{$WARN FOR_LOOP_VAR_UNDEF OFF}
procedure Hidden(Last: Integer);
var
  I: Integer;
begin
  for I := 2 to Last do
    WriteLn(I);
  WriteLn(I);
end;

{$WARN FOR_LOOP_VAR_UNDEF ON}
procedure Visible(Last: Integer);
var
  I: Integer;
begin
  for I := 2 to Last do
    WriteLn(I);
  WriteLn(I);
end;

begin
  Hidden(5);
  Visible(5);
end.
