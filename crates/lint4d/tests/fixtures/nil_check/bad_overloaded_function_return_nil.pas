unit bad_overloaded_function_return_nil;
interface
implementation
uses System;

function MaybeGetObject(Flag: Boolean): TObject; overload;
begin
  if Flag then
    Result := TObject.Create
  else
    Result := nil;
end;

function MaybeGetObject(const Key: string): TObject; overload;
begin
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
