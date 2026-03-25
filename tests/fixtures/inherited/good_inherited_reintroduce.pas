unit GoodInheritedReintroduce;

interface

type
  TReintroducedCtor = class
  public
    constructor Create; reintroduce;
    destructor Destroy; override;
  end;

  TReintroducedDtor = class
  public
    constructor Create;
    destructor Destroy; reintroduce;
  end;

implementation

{ reintroduced constructor — no inherited needed, no warn }
constructor TReintroducedCtor.Create;
begin
  FValue := 1;
end;

destructor TReintroducedCtor.Destroy;
begin
  FValue := 0;
  inherited;
end;

constructor TReintroducedDtor.Create;
begin
  inherited;
  FValue := 1;
end;

{ reintroduced destructor — no inherited needed, no warn }
destructor TReintroducedDtor.Destroy;
begin
  FValue := 0;
end;

end.
