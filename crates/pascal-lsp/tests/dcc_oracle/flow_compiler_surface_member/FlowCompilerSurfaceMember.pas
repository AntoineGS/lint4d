program FlowCompilerSurfaceMember;

type
  TSample = class
  end;

procedure Demo;
var
  Dead: Integer;
  Instance: TSample;
begin
  Dead := 1;
  Instance := TSample.Create;
  Instance.Free;
end;

begin
  Demo;
end.
