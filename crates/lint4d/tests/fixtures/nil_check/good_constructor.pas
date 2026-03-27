unit good_constructor;
interface
implementation
uses System;
procedure Test;
var
  Obj: TObject;
begin
  Obj := TObject.Create;
  Obj.ClassName;
  Obj.Free;
end;
end.
