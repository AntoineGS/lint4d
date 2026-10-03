unit bad_use_after_free_in_for_in_body;
interface
implementation
procedure Test(List: TObjectList);
var
  Item: TObject;
begin
  for Item in List do
  begin
    Item.Free;
    Item.Foo;
  end;
end;
end.
