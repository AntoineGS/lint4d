program FlowSurfaceIncompleteProvider;

uses FlowSurfaceProvider;

procedure Demo;
var
  Dead: Integer;
  Instance: TSurfaceObject;
begin
  Dead := 1;
  Instance.Free;
end;

begin
  Demo;
end.
