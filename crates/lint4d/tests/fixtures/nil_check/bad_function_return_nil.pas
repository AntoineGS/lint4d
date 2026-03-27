unit bad_function_return_nil;
interface
implementation
uses System;

function MaybeGetObject(Flag: Boolean): TObject;
begin
  if Flag then
    Result := TObject.Create
  else
    Result := nil;
end;

procedure Test;
var
  Obj: TObject;
begin
  Obj := MaybeGetObject(True);
  Obj.ClassName;
end;
end.
