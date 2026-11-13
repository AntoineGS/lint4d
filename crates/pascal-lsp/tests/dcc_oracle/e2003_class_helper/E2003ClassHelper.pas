unit E2003ClassHelper;
interface
implementation
type
  TObject = class end;
  TFoo = class(TObject) end;
  TFooHelper = class helper for TFoo
    procedure Extra;
  end;
procedure TFooHelper.Extra;
begin
end;
procedure P;
var Foo: TFoo;
begin
  Foo.Extra;
  Foo.Nope;
end;
end.
