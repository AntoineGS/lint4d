program RaisePath;
uses SysUtils;
function Answer(Flag: Boolean): Integer;
begin
  if Flag then
    raise Exception.Create('failed');
  Result := 1;
end;
begin
  WriteLn(Answer(False));
end.
