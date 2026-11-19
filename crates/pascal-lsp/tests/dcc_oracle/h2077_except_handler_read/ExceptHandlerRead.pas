unit ExceptHandlerRead;

interface

implementation

uses SysUtils;

procedure P;
var
  Value: Integer;
begin
  Value := 1;
  try
    raise Exception.Create('forced');
  except
    Writeln(Value);
  end;
end;

end.
