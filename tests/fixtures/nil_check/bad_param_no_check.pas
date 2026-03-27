unit bad_param_no_check;
interface
implementation
uses System;
procedure Test(AObj: TObject);
begin
  AObj.ClassName;
end;
end.
