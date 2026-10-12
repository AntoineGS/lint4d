unit D;
interface
implementation
procedure Hidden;
var
  L: TStringList;
begin
  L := TStringList.Create;
  {$IF DEFINED(X)}
  rappel: developper en 32 bits pour plus de stabilite
  {$IFEND}
  L.Free;
end;
procedure Broken;
begin
  Foo(;
end;
end.
