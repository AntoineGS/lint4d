program ClassReference;
type
  TWorker = class(TObject)
  end;
procedure Demo;
var
  Worker: TWorker;
begin
  if Worker = nil then
    WriteLn('nil');
end;
begin
  Demo;
end.
