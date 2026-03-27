unit good_function_return_safe;
interface
implementation
uses System;

function CreateObject: TObject;
begin
  Result := TObject.Create;
end;

procedure Test;
var
  Obj: TObject;
begin
  Obj := CreateObject;
  Obj.ClassName;
end;
end.
