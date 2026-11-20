program InactiveBranch;

procedure Demo;
var
  InactiveOnly: Integer;
begin
{$IFDEF LINT4D_NEVER_DEFINED}
  if InactiveOnly = 0 then
    WriteLn('unreachable');
{$ENDIF}
end;

begin
  Demo;
end.
