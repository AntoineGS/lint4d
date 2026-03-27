unit BadFactoryUnprotected;

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
  try
    WriteLn('work');
  finally
    aObj.Free;
  end;
end;

end.
