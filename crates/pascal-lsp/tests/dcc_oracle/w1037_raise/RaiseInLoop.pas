program RaiseInLoop;

uses
  SysUtils;

procedure Log(Value: Integer);
begin
end;

procedure Demo;
var
  I: Integer;
begin
  for I := 1 to 3 do
  begin
    Log(I);
    raise Exception.Create('stop');
  end;
  Log(I);
end;

begin
  Demo;
end.
