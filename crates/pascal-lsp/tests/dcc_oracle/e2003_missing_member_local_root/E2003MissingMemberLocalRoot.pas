unit E2003MissingMemberLocalRoot;

interface
type
  TObject = class
  end;
  TFoo = class(TObject)
    procedure Bar;
  end;

implementation
procedure TFoo.Bar;
begin
end;

procedure Run;
var Foo: TFoo;
begin
  Foo.Baz;
  Foo.Bar;
end;

end.
