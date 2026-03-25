unit GoodNotFactory;

interface

implementation

var
  GInstance: TObject;

function GetExisting: TObject;
begin
  Result := GInstance;
end;

procedure TestNoFalsePositive;
var
  aObj: TObject;
begin
  aObj := GetExisting;
  aObj.ToString;
end;

end.
