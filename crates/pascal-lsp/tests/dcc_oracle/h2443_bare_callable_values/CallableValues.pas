unit CallableValues;

interface

implementation

uses ValueProvider;

type
  TIntegerFunction = function: Integer;

procedure P;
var
  F: TIntegerFunction;
  X: Integer;
begin
  F := @InlineFn;
  F := InlineFn;
  X := InlineFn;
end;

end.
