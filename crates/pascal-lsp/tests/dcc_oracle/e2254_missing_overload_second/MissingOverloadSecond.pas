unit MissingOverloadSecond;

interface

procedure Foo; overload;
procedure Foo(Value: Integer);

implementation

procedure Foo; overload;
begin
end;

procedure Foo(Value: Integer);
begin
end;

end.
