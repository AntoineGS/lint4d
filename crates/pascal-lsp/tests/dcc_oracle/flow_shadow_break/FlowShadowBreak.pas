program FlowShadowBreak;

uses ShadowBreak;

procedure Demo;
var
  I, X: Integer;
begin
  for I := 1 to 2 do begin
    X := 1;
    Break;
    Writeln(X);
  end;
end;

begin
  Demo;
end.
