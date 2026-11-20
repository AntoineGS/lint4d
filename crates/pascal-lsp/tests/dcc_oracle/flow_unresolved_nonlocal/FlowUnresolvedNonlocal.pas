program FlowUnresolvedNonlocal;

uses FlowExternal;

procedure Demo;
var
  Value: Integer;
begin
  Value := 1;
  Value := Missing;
end;

begin
  Demo;
end.
