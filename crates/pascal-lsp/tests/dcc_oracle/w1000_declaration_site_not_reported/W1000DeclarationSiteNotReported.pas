unit W1000DeclarationSiteNotReported;

interface

procedure DeprecatedRoutine; deprecated;
procedure CallDeprecatedRoutine;

implementation

procedure DeprecatedRoutine;
begin
end;

procedure CallDeprecatedRoutine;
begin
  DeprecatedRoutine;
end;

end.
