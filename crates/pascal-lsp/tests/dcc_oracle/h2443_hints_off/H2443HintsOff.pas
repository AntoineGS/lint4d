unit H2443HintsOff;

interface

implementation

uses InlProv;

{$HINTS OFF}
procedure P;
begin
  if UsesImplUnit = 0 then;
end;
{$HINTS ON}

end.
