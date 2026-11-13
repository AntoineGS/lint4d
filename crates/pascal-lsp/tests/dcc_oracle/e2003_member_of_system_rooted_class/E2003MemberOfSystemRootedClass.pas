unit E2003MemberOfSystemRootedClass;

interface

type
  TFoo = class
    procedure Bar;
  end;

implementation

procedure TFoo.Bar;
begin
end;

procedure Probe;
var
  Foo: TFoo;
begin
  Foo.Free;
  Foo.ClassName;
  Foo.Nope;
end;

end.
