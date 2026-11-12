unit IdenticalResult;

interface

type
  TThing = class
    class function Foo(Value: Integer): Integer; overload; virtual; abstract;
    class function Foo(Value: Integer): Integer; overload; virtual; abstract;
  end;

implementation

end.
