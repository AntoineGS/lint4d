unit AnonymousMethod;

interface

type
  TProc = reference to procedure;
  TFunc = reference to function(const X: Integer): Integer;

implementation

procedure TestAnonymous;
var
  p: TProc;
  f: TFunc;
begin
  p := procedure
    begin
      WriteLn('hello');
    end;

  f := function(const X: Integer): Integer
    begin
      Result := X * 2;
    end;

  p;
  WriteLn(f(21));
end;

end.
