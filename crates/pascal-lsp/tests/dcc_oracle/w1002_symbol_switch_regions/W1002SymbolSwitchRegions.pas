unit W1002SymbolSwitchRegions;

interface

procedure PlatformRoutine; platform;
procedure CallPlatformRoutine;

implementation

procedure PlatformRoutine;
begin
end;

procedure CallPlatformRoutine;
begin
{$WARN SYMBOL_PLATFORM OFF}
  PlatformRoutine;
{$WARN SYMBOL_PLATFORM ON}
  PlatformRoutine;
end;

end.
