unit bad_double_free_in_first_overload;
interface
procedure Foo(A: Integer); overload;
procedure Foo(const S: string); overload;
implementation
procedure Foo(A: Integer);
var
  aObj: TObject;
begin
  aObj := TObject.Create;
  try
  finally
    aObj.Free;
    aObj.Free;
  end;
end;

procedure Foo(const S: string);
var
  aObj: TObject;
begin
  aObj := TObject.Create;
  try
  finally
    aObj.Free;
  end;
end;
end.
