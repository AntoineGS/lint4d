unit good_function_return_after_call;
interface
implementation
uses System;

function CreateObject: TObject;
begin
  Setup;
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
