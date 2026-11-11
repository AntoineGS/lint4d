unit IdenticalParamsUnmarked;

interface

type
  TThing = class
    function Foo(Value: Integer): Integer; virtual; abstract;
    function Foo(Value: Integer): string; virtual; abstract;
  end;

implementation

end.
