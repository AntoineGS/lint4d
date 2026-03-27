unit good_comparison;
interface
implementation
uses System;
procedure TestNotNil(AObj: TObject);
begin
  if AObj <> nil then
    AObj.ClassName;
end;

procedure TestAssigned(AObj: TObject);
begin
  if Assigned(AObj) then
    AObj.ClassName;
end;
end.
