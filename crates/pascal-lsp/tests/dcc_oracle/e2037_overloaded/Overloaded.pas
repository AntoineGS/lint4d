unit Overloaded;

interface

procedure Foo(Value: Integer); overload;
procedure Foo(Value: string); overload;

implementation

procedure Foo(Value: Boolean);
begin
end;

end.
