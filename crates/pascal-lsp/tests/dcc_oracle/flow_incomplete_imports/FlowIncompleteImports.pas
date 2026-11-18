program FlowIncompleteImports;

uses FlowExternal;

procedure Demo;
var
  Unused: Integer;
  WrittenOnly: Integer;
begin
  WrittenOnly := ExternalValue;
end;

begin
  Demo;
end.
