program FlowLaterUsesShadowedExternalValue;

uses Indexed, Shadow;

procedure Demo;
var
  WrittenOnly: Integer;
begin
  WrittenOnly := ExternalValue;
end;

begin
  Demo;
end.
