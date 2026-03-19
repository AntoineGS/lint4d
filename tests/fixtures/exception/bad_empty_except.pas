unit BadEmptyExcept;

interface

implementation

procedure DoRisky;
begin
  try
    WriteLn('risky');
  except
  end;
end;

end.
