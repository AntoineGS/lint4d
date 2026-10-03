unit bad_double_free_in_overloaded_method;
interface
type
  TFoo = class
  public
    procedure Bar(A: Integer); overload;
    procedure Bar(const S: string); overload;
  end;
implementation
procedure TFoo.Bar(A: Integer);
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

procedure TFoo.Bar(const S: string);
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
