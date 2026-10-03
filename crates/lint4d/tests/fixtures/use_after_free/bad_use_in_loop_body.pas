unit bad_use_in_loop_body;
interface
implementation
procedure Test(Cond: Boolean);
var
  Obj: TObject;
  Other: TObject;
begin
  Obj := TObject.Create;
  Obj.Free;
  while Cond do
  begin
    Obj.Foo;
    Other := TObject.Create;
    Other.Free;
  end;
end;
end.
