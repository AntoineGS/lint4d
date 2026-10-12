unit bad_overload_selected_by_arity;
interface
uses System;
function GetDefault(A: Integer; B: Integer = 0): TObject; overload;
function GetDefault(const S, T: string): TObject; overload;
implementation

function GetObject(Flag: Boolean): TObject; overload;
begin
  Result := TObject.Create;
end;

function GetObject(const Key: string; Index: Integer): TObject; overload;
begin
  Result := nil;
end;

function GetDefault(A: Integer; B: Integer): TObject;
begin
  Result := nil;
end;

function GetDefault(const S, T: string): TObject;
begin
  Result := TObject.Create;
end;

procedure Test;
var
  Obj: TObject;
  Obj2: TObject;
  Obj3: TObject;
  Obj4: TObject;
begin
  Obj := GetObject('key', 1);
  Obj.ClassName;
  Obj2 := GetObject(True);
  Obj2.ClassName;
  Obj3 := GetDefault(1);
  Obj3.ClassName;
  Obj4 := GetDefault(1, 2);
  Obj4.ClassName;
end;
end.
