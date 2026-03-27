unit bad_use_after_freeandnil;
interface
implementation
uses System;
procedure Test(AObj: TObject);
begin
  RaiseIfNil(AObj, 'AObj');
  AObj.ClassName;
  FreeAndNil(AObj);
  AObj.ClassName;
end;
end.
