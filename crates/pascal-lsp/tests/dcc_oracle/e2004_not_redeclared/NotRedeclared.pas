unit NotRedeclared;

interface

type
  TFoo = class;
  TFoo = class
    Value: Integer;
  end;
  TFoo<T> = class
  end;

procedure Plain;
procedure Overloaded; overload;
procedure Overloaded(Value: Integer); overload;

implementation

procedure Plain;
begin
end;

procedure Overloaded; overload;
begin
end;

procedure Overloaded(Value: Integer); overload;
begin
end;

end.
