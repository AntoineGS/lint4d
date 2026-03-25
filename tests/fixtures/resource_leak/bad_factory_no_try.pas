unit BadFactoryNoTry;

interface

implementation

function CreateObject: TObject;
begin
  Result := TObject.Create;
end;

procedure TestLeak;
var
  aObj: TObject;
begin
  aObj := CreateObject;
  aObj.ToString;
  aObj.Free;
end;

end.
