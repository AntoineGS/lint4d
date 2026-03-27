unit BadLocalVarCamel;

interface

implementation

procedure DoWork(BadParam: Integer; const AnotherParam: string);
var
  MyCounter: Integer;
  AnotherBadName: string;
  x: Integer;
begin
  MyCounter := BadParam;
  AnotherBadName := AnotherParam;
  x := 2;
end;

end.
