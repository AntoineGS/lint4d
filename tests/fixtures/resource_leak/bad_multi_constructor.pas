unit BadMultiConstructor;

interface

implementation

procedure TestMulti;
var
  obj1, obj2: TObject;
begin
  obj1 := TObject.Create;
  obj2 := TObject.Create;
  obj1.ToString;
  try
    WriteLn('work');
  finally
    obj1.Free;
    obj2.Free;
  end;
end;

end.
