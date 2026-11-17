unit W1000AllOverloadsDeprecated;

interface

procedure Overloaded(Value: Integer); overload; deprecated 'replace Overloaded';
procedure Overloaded(Value: string); overload; deprecated 'replace Overloaded';
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
