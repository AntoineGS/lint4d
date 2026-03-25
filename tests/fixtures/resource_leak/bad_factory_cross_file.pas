unit BadFactoryCrossFile;

interface

implementation

uses FactoryUnit;

procedure TestLeak;
var
  aObj: TObject;
begin
  aObj := CreateWidget;
  aObj.ToString;
  aObj.Free;
end;

end.
