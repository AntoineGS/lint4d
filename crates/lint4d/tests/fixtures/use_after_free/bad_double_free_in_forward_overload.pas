unit bad_double_free_in_forward_overload;
interface
implementation
procedure Foo(A: Integer); overload; forward;
procedure Foo(const S: string); overload; forward;

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
