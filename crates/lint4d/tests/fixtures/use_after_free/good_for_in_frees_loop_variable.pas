unit good_for_in_frees_loop_variable;
interface
implementation
procedure Test(List: TObjectList);
var
  Item: TObject;
begin
  for Item in List do
    Item.Free;
end;
end.
