unit GoodLocalVarPascal;

interface

implementation

procedure DoWork;
var
  MyCounter: Integer;
  AnotherName: string;
  X: Integer;
  I: Integer;
begin
  MyCounter := 1;
  AnotherName := 'test';
  for I := 0 to 10 do
    X := I;
end;

end.
