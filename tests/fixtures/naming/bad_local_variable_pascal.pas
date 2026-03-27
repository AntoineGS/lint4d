unit BadLocalVarPascal;

interface

implementation

procedure DoWork(badParam: Integer; const anotherParam: string);
var
  myCounter: Integer;
  anotherBad: string;
  x: Integer;
begin
  myCounter := badParam;
  anotherBad := anotherParam;
  x := 2;
end;

end.
