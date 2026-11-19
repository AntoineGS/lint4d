program VarArgNotDead;

procedure Touch(var Value: Integer);
begin
  Value := Value + 1;
end;

procedure Demo;
var
  Value: Integer;
begin
  Value := 1;
  Touch(Value);
end;

begin
  Demo;
end.
