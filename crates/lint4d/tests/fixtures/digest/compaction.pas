unit Compaction;
interface
implementation
procedure Compact(A: Integer; Items: TStringList);
var
  Q: TQuery;
begin
  Log('start'); Open;
  if A > 0 then
    Total := Total + 1;
  if A > 1 then
    for var I := 0 to A do
      Count := Count + I;
  Log('mid');
  if Ready(A) then
    Total := 0;
  if (SomeVeryLongVariableName > 0) and (AnotherVeryLongVariableName < 10) and IsReady(A) then
    Total := 2;
  if A = 1 then
    Items.Clear
  else
    Items.Sort;
  case A of
    1: Total := 1;
    2: if Items.Count > 0 then Items.Delete(0);
    3: Total := 3;
  end;
  try
    Total := 4;
  except
  end;
  try
    Q := TQuery.Create;
    Save(Q);
  except
  end;
  Log('end');
end;
end.
