unit MissingOverloadDefinition;

interface

procedure Foo; overload;
procedure Foo(Value: Integer); overload;

implementation

procedure Foo;
begin
end;

procedure Foo(Value: Integer); overload;
begin
end;

end.
