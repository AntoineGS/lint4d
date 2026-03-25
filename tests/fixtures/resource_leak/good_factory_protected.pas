unit GoodFactoryProtected;

interface

implementation

function CreateObject: TObject;
begin
  Result := TObject.Create;
end;

procedure TestSafe;
var
  aObj: TObject;
begin
  aObj := CreateObject;
  try
    aObj.ToString;
  finally
    aObj.Free;
  end;
end;

end.
