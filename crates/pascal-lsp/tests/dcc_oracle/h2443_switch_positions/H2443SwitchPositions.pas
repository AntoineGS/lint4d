unit H2443SwitchPositions;

interface

implementation

uses InlProv;

procedure CallThenEnableBeforeProcedureEnd;
begin
  {$HINTS OFF}
  if UsesImplUnit = 0 then;
  {$HINTS ON}
end;

procedure CallThenEnableBeforeUnitEnd;
begin
  {$HINTS OFF}
  if UsesImplUnit = 0 then;
end;
{$HINTS ON}

procedure EnableBeforeCall;
begin
  {$HINTS OFF}
  {$HINTS ON}
  if UsesImplUnit = 0 then;
end;

end.
