program RaiseInLoop;

uses
  SysUtils;

procedure Demo;
var
  I: Integer;
begin
  for I := 1 to 3 do
  begin
    WriteLn(I);
    raise Exception.Create('stop');
  end;
  WriteLn(I);
end;

begin
  Demo;
end.
