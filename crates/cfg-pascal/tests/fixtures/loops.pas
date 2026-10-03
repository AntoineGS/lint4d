unit LoopTest;
interface
implementation

procedure TestForLoop;
var
  I: Integer;
begin
  for I := 0 to 10 do
    I := I + 1;
end;

procedure TestWhileLoop;
var
  X: Integer;
begin
  X := 0;
  while X < 10 do
    X := X + 1;
end;

procedure TestRepeatUntil;
var
  X: Integer;
begin
  X := 0;
  repeat
    X := X + 1;
  until X >= 10;
end;

procedure TestBreakInLoop;
var
  I: Integer;
begin
  for I := 0 to 10 do
  begin
    if I = 5 then
      Break;
    I := I + 1;
  end;
end;

end.
