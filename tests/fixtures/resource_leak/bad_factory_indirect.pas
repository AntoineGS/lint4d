unit BadFactoryIndirect;

interface

implementation

function CreateInner: TObject;
begin
  Result := TObject.Create;
end;

function CreateOuter: TObject;
begin
  Result := CreateInner;
end;

procedure TestLeak;
var
  aObj: TObject;
begin
  aObj := CreateOuter;
  aObj.ToString;
  aObj.Free;
end;

end.
