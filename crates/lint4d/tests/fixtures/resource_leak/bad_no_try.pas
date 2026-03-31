unit BadNoTry;

interface

implementation

procedure TestNoTry;
var
  Obj: TObject;
begin
  Obj := TObject.Create;
  Obj.ToString;
  Obj.Free;
end;

end.
