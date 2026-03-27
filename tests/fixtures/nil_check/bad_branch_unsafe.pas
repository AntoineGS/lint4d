unit bad_branch_unsafe;
interface
implementation
uses System;
procedure Test(AObj: TObject; Flag: Boolean);
begin
  if Flag then
  begin
    if AObj <> nil then
      AObj.ClassName;
  end
  else
  begin
    AObj.ClassName;
  end;
end;
end.
