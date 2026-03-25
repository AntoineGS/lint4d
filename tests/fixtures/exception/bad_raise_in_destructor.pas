unit BadRaiseInDestructor;

interface

type
  TDirectRaise = class
  public
    destructor Destroy; override;
  end;

  TRaiseInFinally = class
  public
    destructor Destroy; override;
  end;

  TBareReraise = class
  public
    destructor Destroy; override;
  end;

  TNestedProcRaise = class
  public
    destructor Destroy; override;
  end;

implementation

{ Direct raise in destructor body — should warn }
destructor TDirectRaise.Destroy;
begin
  raise Exception.Create('oops');
  inherited;
end;

{ Raise inside try..finally (no except) — should warn }
destructor TRaiseInFinally.Destroy;
begin
  try
    raise Exception.Create('still escapes');
  finally
    // cleanup only
  end;
  inherited;
end;

{ Bare re-raise in destructor — should warn }
destructor TBareReraise.Destroy;
begin
  raise;
  inherited;
end;

{ Raise inside nested procedure — should warn }
destructor TNestedProcRaise.Destroy;

  procedure CleanupHelper;
  begin
    raise Exception.Create('nested raise escapes');
  end;

begin
  CleanupHelper;
  inherited;
end;

end.
