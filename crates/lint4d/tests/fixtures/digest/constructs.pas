unit T;
interface
implementation
procedure Outer(A: Integer);
  procedure Inner;
  begin
    Bar;
  end;
begin
  for var I := 0 to A do
    Inner;
  while Items.Count > 0 do
    Items.Delete(0);
  with TBOField(Fields[0]) do
    Clear;
  try
    inherited;
  except
    on E: EAbort do
      raise;
    on E: Exception do
      Log(E.Message);
  end;
  {$IFDEF DEBUG}
  Trace('x');
  {$ELSE}
  Trace('y');
  {$ENDIF}
  TThread.Queue(nil, procedure begin Ping; end);
  if A = 1 then
    Bar
  else if A = 2 then
    Baz;
end;
end.
