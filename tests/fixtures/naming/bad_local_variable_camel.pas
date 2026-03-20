unit BadLocalVarCamel;

interface

implementation

procedure DoWork;
var
  MyCounter: Integer;
  AnotherBadName: string;
  x: Integer;
begin
  MyCounter := 1;
  AnotherBadName := 'test';
  x := 2;
end;

end.
