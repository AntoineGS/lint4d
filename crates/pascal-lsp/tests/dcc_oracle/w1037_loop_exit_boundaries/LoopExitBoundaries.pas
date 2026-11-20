program LoopExitBoundaries;

procedure PostOnly(Last: Integer);
var
  I: Integer;
begin
  for I := 2 to Last do
    ;
  WriteLn(I);
end;

procedure Nonempty;
var
  I: Integer;
begin
  for I := 1 to 2 do
    ;
  WriteLn(I);
end;

begin
  PostOnly(5);
  Nonempty;
end.
