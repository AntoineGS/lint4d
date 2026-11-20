program FlowShadowExit;

uses ShadowExit;

procedure Demo;
var
  X: Integer;
begin
  X := 1;
  Exit;
  Writeln(X);
end;

begin
  Demo;
end.
