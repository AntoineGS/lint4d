program FlowH2164ErrorInCallArgument;

procedure Foo(const Value: Integer);
begin
end;

procedure Demo;
var
  U: Integer;
begin
  Foo(Missing);
end;

begin
  Demo;
end.
