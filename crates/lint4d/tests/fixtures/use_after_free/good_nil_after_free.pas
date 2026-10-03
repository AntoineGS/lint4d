unit good_nil_after_free;
interface
implementation
procedure Test;
var
  Obj: TObject;
begin
  Obj := TObject.Create;
  Obj.Free;
  Obj := nil;
end;
end.
