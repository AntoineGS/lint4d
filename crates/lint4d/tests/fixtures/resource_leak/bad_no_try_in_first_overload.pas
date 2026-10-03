unit BadNoTryInFirstOverload;

interface

procedure Foo(A: Integer); overload;
procedure Foo(const S: string); overload;

implementation

procedure Foo(A: Integer);
var
  Obj: TObject;
begin
  Obj := TObject.Create;
  Obj.ToString;
  Obj.Free;
end;

procedure Foo(const S: string);
var
  Obj: TObject;
begin
  Obj := TObject.Create;
  try
    Obj.ToString;
  finally
    Obj.Free;
  end;
end;

end.
