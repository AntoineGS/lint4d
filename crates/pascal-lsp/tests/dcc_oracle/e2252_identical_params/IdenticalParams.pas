unit IdenticalParams;

interface

type
  TThing = class
    function Foo(Value: Integer): Integer; overload; virtual; abstract;
    function Foo(Value: Integer): string; overload; virtual; abstract;
  end;

implementation

end.
