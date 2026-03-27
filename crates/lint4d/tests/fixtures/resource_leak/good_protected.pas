unit GoodProtected;

interface

implementation

procedure TestSafe;
var
  obj: TObject;
begin
  obj := TObject.Create;
  try
    obj.ToString;
    WriteLn('work');
  finally
    obj.Free;
  end;
end;

end.
