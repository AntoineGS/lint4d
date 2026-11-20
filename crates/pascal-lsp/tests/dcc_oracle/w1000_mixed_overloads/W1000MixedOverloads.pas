unit W1000MixedOverloads;

interface

procedure Overloaded(Value: Integer); overload; deprecated;
procedure Overloaded(Value: string); overload;
procedure CallOverloads;

implementation

procedure Overloaded(Value: Integer);
begin
end;

procedure Overloaded(Value: string);
begin
end;

procedure CallOverloads;
begin
  Overloaded(1);
  Overloaded('a');
end;

end.
