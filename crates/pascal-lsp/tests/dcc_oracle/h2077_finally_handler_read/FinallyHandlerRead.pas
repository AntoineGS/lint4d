unit FinallyHandlerRead;

interface

implementation

procedure P;
var
  Value: Integer;
begin
  Value := 1;
  try
    Writeln('body');
  finally
    Writeln(Value);
  end;
end;

end.
