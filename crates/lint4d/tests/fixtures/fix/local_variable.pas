unit LocalVariableFix;

interface

implementation

procedure DoWork(badParam: Integer; const anotherParam: string);
var
  myCounter: Integer;
  anotherBadName: string;
  x: Integer;
begin
  myCounter := badParam;
  anotherBadName := anotherParam;
  x := 2;
end;

end.
