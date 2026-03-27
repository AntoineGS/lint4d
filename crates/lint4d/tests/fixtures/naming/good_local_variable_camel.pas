unit GoodLocalVarCamel;

interface

implementation

procedure DoWork(goodParam: Integer; const anotherParam: string);
var
  myCounter: Integer;
  anotherName: string;
  x: Integer;
  i: Integer;
begin
  myCounter := goodParam;
  anotherName := anotherParam;
  for i := 0 to 10 do
    x := i;
end;

end.
