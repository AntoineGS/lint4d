program NestedRoutineLocal;

procedure Outer;
var
  Value: Integer;
  procedure AssignValue;
  begin
    Value := 1;
  end;
begin
  AssignValue;
end;

begin
  Outer;
end.
