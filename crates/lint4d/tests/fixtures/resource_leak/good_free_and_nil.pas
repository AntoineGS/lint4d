unit GoodFreeAndNil;

interface

implementation

procedure TestFreeAndNil;
var
  obj: TObject;
begin
  obj := TObject.Create;
  try
    obj.ToString;
  finally
    FreeAndNil(obj);
  end;
end;

end.
