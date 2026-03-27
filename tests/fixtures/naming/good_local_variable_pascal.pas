unit GoodLocalVarPascal;

interface

implementation

procedure DoWork(GoodParam: Integer; const AnotherParam: string);
var
  MyCounter: Integer;
  AnotherName: string;
  X: Integer;
  I: Integer;
begin
  MyCounter := GoodParam;
  AnotherName := AnotherParam;
  for I := 0 to 10 do
    X := I;
end;

end.
