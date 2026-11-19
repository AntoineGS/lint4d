program SwitchW1035;

{$WARN NO_RETVAL OFF}
function Hidden: Integer;
begin
end;

{$WARN NO_RETVAL ON}
function Visible: Integer;
begin
end;

begin
  WriteLn(Hidden);
  WriteLn(Visible);
end.
