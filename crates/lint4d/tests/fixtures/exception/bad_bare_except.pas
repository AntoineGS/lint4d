unit BadBareExcept;

interface

implementation

procedure DoRisky;
begin
  try
    WriteLn('risky');
  except
    WriteLn('caught something');
  end;
end;

end.
