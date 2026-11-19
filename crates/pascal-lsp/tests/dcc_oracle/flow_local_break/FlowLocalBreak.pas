program FlowLocalBreak;

procedure Consume(Value: Integer);
begin
end;

procedure Demo;
var
  Break: Integer;
begin
  Break := 1;
  Consume(Break);
end;

begin
  Demo;
end.
