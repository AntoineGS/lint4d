unit W1000ErrorPromotion;

interface

procedure DeprecatedRoutine; deprecated;

var
  DeprecatedLibraryVariable: Integer library;

procedure PlatformRoutine; platform;
procedure ExperimentalRoutine; experimental;
procedure UseHints;

implementation

procedure DeprecatedRoutine;
begin
end;

procedure PlatformRoutine;
begin
end;

procedure ExperimentalRoutine;
begin
end;

procedure UseHints;
var
  Value: Integer;
begin
  DeprecatedRoutine;
  Value := DeprecatedLibraryVariable;
  PlatformRoutine;
  ExperimentalRoutine;
end;

end.
