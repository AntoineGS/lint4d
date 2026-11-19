program FlowSelfReceiver;

type
  TSample = class
    procedure CheckSelf;
  end;

procedure TSample.CheckSelf;
var
  Unused: Integer;
begin
  if Self = nil then
    Exit;
end;

begin
end.
