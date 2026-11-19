program FlowNestedExit;

procedure Consume(Value: Integer);
begin
end;

procedure Demo;
var
  Value: Integer;
  procedure Exit;
  begin
  end;
begin
  Value := 1;
  Exit;
  Consume(Value);
end;

begin
  Demo;
end.
