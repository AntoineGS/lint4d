unit C;
interface
implementation
procedure Deep(A: Integer);
begin
  Start;
  if A > 0 then
  begin
    One;
    if A > 1 then
    begin
      Two;
      if A > 2 then
      begin
        Three;
        if A > 3 then
          Four;
      end;
    end;
  end;
  Finish;
end;
end.
