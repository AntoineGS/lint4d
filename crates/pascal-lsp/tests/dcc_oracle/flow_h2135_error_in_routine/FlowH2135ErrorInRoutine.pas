program FlowH2135ErrorInRoutine;

procedure Demo;
begin
  while False do WriteLn('unreachable');
  Missing;
end;

begin
  Demo;
end.
