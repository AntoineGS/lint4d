unit W1000InsideDeclaringUnit;

interface

procedure DeprecatedRoutine; deprecated;
procedure CallFromSameUnit;

implementation

procedure DeprecatedRoutine;
begin
end;

procedure CallFromSameUnit;
begin
  DeprecatedRoutine;
end;

end.
