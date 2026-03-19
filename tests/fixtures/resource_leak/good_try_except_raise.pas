unit GoodTryExceptRaise;

interface

implementation

procedure TestExceptRaise;
var
  obj: TObject;
begin
  obj := TObject.Create;
  try
    obj.DoWork;
  except
    obj.Free;
    raise;
  end;
end;

end.
