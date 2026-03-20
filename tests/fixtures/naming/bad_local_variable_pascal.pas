unit BadLocalVarPascal;

interface

implementation

procedure DoWork;
var
  myCounter: Integer;
  anotherBad: string;
  x: Integer;
begin
  myCounter := 1;
  anotherBad := 'test';
  x := 2;
end;

end.
