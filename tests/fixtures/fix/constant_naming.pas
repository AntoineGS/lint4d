unit ConstantNamingFix;

interface

const
  maxSize = 100;
  httpPort = 8080;
  ALREADY_GOOD = 42;

implementation

procedure DoWork;
var
  x: Integer;
begin
  x := maxSize + httpPort + ALREADY_GOOD;
end;

end.
