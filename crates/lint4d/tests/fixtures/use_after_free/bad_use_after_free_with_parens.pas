unit bad_use_after_free_with_parens;
interface
implementation
procedure Test;
var
  Obj: TObject;
begin
  Obj := TObject.Create;
  Obj.Free();
  Obj.Foo;
end;
end.
