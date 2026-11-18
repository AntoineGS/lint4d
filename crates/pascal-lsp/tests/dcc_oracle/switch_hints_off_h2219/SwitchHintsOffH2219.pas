unit SwitchHintsOffH2219;

interface

type
  TUnused = class
  private
{$HINTS OFF}
    FBeforeFieldSwitch: Integer;
{$HINTS ON}
    FBeforeEndSwitch: Integer;
  end;

implementation

{$HINTS OFF}
end.
