unit DprojWarningsOff;

interface

procedure DeprecatedRoutine; deprecated;
procedure UseDeprecatedRoutine;

implementation

procedure DeprecatedRoutine;
begin
end;

procedure UseDeprecatedRoutine;
begin
  DeprecatedRoutine;
end;

end.
