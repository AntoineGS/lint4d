unit SimpleTest;
interface
implementation

procedure SimpleLinear;
var
  X: Integer;
begin
  X := 1;
  X := X + 1;
end;

procedure SimpleIfElse(Cond: Boolean);
var
  X: Integer;
begin
  if Cond then
    X := 1
  else
    X := 2;
  X := X + 1;
end;

procedure SimpleRaise;
begin
  raise Exception.Create('error');
end;

procedure SimpleExit;
var
  X: Integer;
begin
  X := 1;
  if X = 0 then
    Exit;
  X := 2;
end;

end.
