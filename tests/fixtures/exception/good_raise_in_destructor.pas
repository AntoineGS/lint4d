unit GoodRaiseInDestructor;

interface

type
  TGuardedRaise = class
  public
    destructor Destroy; override;
  end;

  TGuardedExceptInFinally = class
  public
    destructor Destroy; override;
  end;

  TRegularMethod = class
  public
    procedure DoSomething;
  end;

  TConstructorRaise = class
  public
    constructor Create;
  end;

  TCleanDestructor = class
  public
    destructor Destroy; override;
  end;

implementation

{ Raise inside try..except — guarded, no warn }
destructor TGuardedRaise.Destroy;
begin
  try
    raise Exception.Create('caught');
  except
    on E: Exception do
      ; // swallowed
  end;
  inherited;
end;

{ Raise inside nested try..except within try..finally — has except, guarded, no warn }
destructor TGuardedExceptInFinally.Destroy;
begin
  try
    try
      raise Exception.Create('caught');
    except
      on E: Exception do
        ; // swallowed
    end;
  finally
    // cleanup
  end;
  inherited;
end;

{ Raise in a regular method — not a destructor, no warn }
procedure TRegularMethod.DoSomething;
begin
  raise Exception.Create('this is fine');
end;

{ Raise in a constructor — not a destructor, no warn }
constructor TConstructorRaise.Create;
begin
  inherited;
  raise Exception.Create('also fine');
end;

{ Destructor with no raises — no warn }
destructor TCleanDestructor.Destroy;
begin
  FValue := 0;
  inherited;
end;

end.
