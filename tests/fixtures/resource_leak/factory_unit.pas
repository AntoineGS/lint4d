unit FactoryUnit;

interface

function CreateWidget: TObject;

implementation

function CreateWidget: TObject;
begin
  Result := TObject.Create;
end;

end.
