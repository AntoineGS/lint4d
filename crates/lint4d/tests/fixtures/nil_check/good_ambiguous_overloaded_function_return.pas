unit good_ambiguous_overloaded_function_return;
interface
implementation
uses System;

function GetObject(Flag: Boolean): TObject; overload;
begin
  Result := TObject.Create;
end;

function GetObject(const Key: string): TObject; overload;
begin
  Result := nil;
end;

procedure Test;
var
  Obj: TObject;
begin
  Obj := GetObject(True);
  Obj.ClassName;
end;
end.
