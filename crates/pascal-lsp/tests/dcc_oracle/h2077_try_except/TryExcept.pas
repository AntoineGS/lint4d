program TryExcept;

procedure MaybeRaise;
begin
end;

procedure Demo;
var
  Value: Integer;
begin
  Value := 1;
  try
    Value := 2;
    MaybeRaise;
  except
    Value := 3;
  end;
  WriteLn(Value);
end;

begin
  Demo;
end.
