unit GoodBareExceptWithRaise;

interface

implementation

procedure DoRisky;
begin
  try
    WriteLn('risky');
  except
    WriteLn('cleanup');
    raise;
  end;
end;

end.
