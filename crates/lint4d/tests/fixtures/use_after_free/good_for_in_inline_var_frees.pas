unit good_for_in_inline_var_frees;
interface
implementation
procedure Test(List: TObjectList);
begin
  for var Item in List do
    Item.Free;
end;
end.
