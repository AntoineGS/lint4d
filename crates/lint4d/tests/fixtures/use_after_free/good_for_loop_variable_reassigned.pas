unit good_for_loop_variable_reassigned;
interface
implementation
procedure Test(List: TObjectList);
var
  Item: TObject;
begin
  Item := TObject.Create;
  Item.Free;
  for Item in List do
    Item.Foo;
  Item.Free;
  for Item := First to Last do
    Item.Foo;
end;
end.
