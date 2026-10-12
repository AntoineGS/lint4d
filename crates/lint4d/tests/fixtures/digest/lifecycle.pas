unit Lifecycle;
interface
implementation
procedure Foo(A: Integer);
var
  L: TStringList;
begin
  Log('start'); Log('again'); Prepare;
  L := TStringList.Create;
  try
    case A of
      1: Bar;
      2: begin Baz; Qux; end;
    else
      raise Exception.Create('bad');
    end;
    if A > 0 then
      Exit;
    DB.StartTransaction;
    X := 1;
  finally
    L.Free;
  end;
end;
procedure W;
var
  L: TStringList;
begin
  try
    L := TStringList.Create;
    Db.Open;
    try
      L.Add('x');
    finally
      L.Free;
    end;
  except
    on E: Exception do
      Log(E.Message);
  end;
end;
function F: TList;
var
  L: TList;
begin
  Result := TList.Create;
  FItems := TList.Create;
  L := TList.Create;
end;
end.
