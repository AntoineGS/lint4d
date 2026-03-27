unit good_raiseifnil;
interface
implementation
uses System;
procedure Test(AObj: TObject);
begin
  RaiseIfNil(AObj, 'AObj');
  AObj.ClassName;
end;
end.
