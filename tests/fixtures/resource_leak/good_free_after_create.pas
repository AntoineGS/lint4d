unit GoodFreeAfterCreate;

interface

implementation

procedure TestFreeAfterCreate;
var
  aObj: TObject;
begin
  aObj := TObject.Create; // inline comment
  aObj.Free;
end;

end.
