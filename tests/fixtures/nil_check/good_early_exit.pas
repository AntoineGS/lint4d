unit good_early_exit;
interface
implementation
uses System;
procedure TestExitGuard(AObj: TObject);
begin
  if AObj = nil then
    Exit;
  AObj.ClassName;
end;

procedure TestRaiseGuard(AObj: TObject);
begin
  if not Assigned(AObj) then
    raise Exception.Create('AObj is nil');
  AObj.ClassName;
end;
end.
