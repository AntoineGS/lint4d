unit good_repeated_unanalyzable_calls;
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

procedure TestExternal;
var
  Obj: TObject;
  Obj2: TObject;
begin
  Obj := ExternalGet(1);
  Obj.ClassName;
  Obj2 := ExternalGet(2);
  Obj2.ClassName;
end;

procedure TestOverloads;
var
  Obj: TObject;
  Obj2: TObject;
begin
  Obj := GetObject(True);
  Obj.ClassName;
  Obj2 := GetObject('key');
  Obj2.ClassName;
end;
end.
