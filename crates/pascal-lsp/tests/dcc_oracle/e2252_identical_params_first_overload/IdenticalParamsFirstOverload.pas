unit IdenticalParamsFirstOverload;

interface

type
  TThing = class
    function Foo(Value: Integer): Integer; overload; virtual; abstract;
    function Foo(Value: Integer): string; virtual; abstract;
  end;

implementation

end.
