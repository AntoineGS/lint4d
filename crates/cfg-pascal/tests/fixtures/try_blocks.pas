unit TryTest;
interface
implementation
uses
  SysUtils;

procedure TestTryFinally;
var
  Obj: TObject;
begin
  Obj := TObject.Create;
  try
    Obj.ClassName;
  finally
    Obj.Free;
  end;
end;

procedure TestTryExcept;
begin
  try
    raise Exception.Create('error');
  except
    on E: Exception do
      E.Message;
  end;
end;

procedure TestNestedTryFinallyExcept;
var
  Obj: TObject;
begin
  Obj := TObject.Create;
  try
    try
      Obj.ClassName;
    except
      on E: Exception do
        Obj.Free;
    end;
  finally
    Obj.Free;
  end;
end;

end.
