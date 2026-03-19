unit TryFinally;

interface

implementation

uses SysUtils;

procedure TestTryFinally;
var
  obj: TObject;
begin
  obj := TObject.Create;
  try
    WriteLn('working');
  finally
    obj.Free;
  end;
end;

procedure TestTryExcept;
begin
  try
    WriteLn('risky');
  except
    on E: Exception do
      WriteLn(E.Message);
  end;
end;

procedure TestEmptyExcept;
begin
  try
    WriteLn('risky');
  except
  end;
end;

procedure TestBareExcept;
begin
  try
    WriteLn('risky');
  except
    WriteLn('caught');
  end;
end;

// NOTE: bare 'raise;' (re-raise without argument) is NOT supported by
// tree-sitter-pascal and produces an ERROR node. This is a known grammar
// limitation. The 'bare-except' rule must account for this when checking
// except blocks that contain raise statements.

end.
