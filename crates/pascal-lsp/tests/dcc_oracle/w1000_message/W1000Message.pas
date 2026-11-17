unit W1000Message;

interface

procedure DeprecatedRoutine; deprecated 'use Other';
procedure CallDeprecated;

implementation

procedure DeprecatedRoutine;
begin
end;

procedure CallDeprecated;
begin
  DeprecatedRoutine;
end;

end.
