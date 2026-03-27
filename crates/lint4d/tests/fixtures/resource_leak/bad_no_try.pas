unit BadNoTry;

interface

implementation

procedure TestNoTry;
var
  obj: TObject;
begin
  obj := TObject.Create;
  obj.ToString;
  obj.Free;
end;

end.
