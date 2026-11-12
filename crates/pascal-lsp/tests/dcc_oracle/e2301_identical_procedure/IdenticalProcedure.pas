unit IdenticalProcedure;

interface

type
  TThing = class
    procedure Foo(Value: Integer); overload; virtual; abstract;
    procedure Foo(Value: Integer); overload; virtual; abstract;
  end;

implementation

end.
