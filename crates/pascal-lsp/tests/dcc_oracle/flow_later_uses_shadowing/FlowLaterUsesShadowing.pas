program FlowLaterUsesShadowing;

uses Indexed, Shadow;

procedure Demo;
var
  X: Integer;
begin
  Fill(X);
  Writeln(X);
end;

begin
  Demo;
end.
