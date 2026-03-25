unit BadInheritedOrder;

interface

type
  TOrderBadCtor = class
  public
    constructor Create;
    destructor Destroy; override;
  end;

  TOrderBadDtor = class
  public
    constructor Create;
    destructor Destroy; override;
  end;

  TOrderBadCtorMiddle = class
  public
    constructor Create;
    destructor Destroy; override;
  end;

implementation

{ inherited at bottom of constructor — should warn }
constructor TOrderBadCtor.Create;
begin
  FValue := 1;
  inherited;
end;

{ inherited correctly last in destructor — no warn }
destructor TOrderBadCtor.Destroy;
begin
  FValue := 0;
  inherited;
end;

{ inherited at top of destructor — should warn }
constructor TOrderBadDtor.Create;
begin
  inherited;
end;

destructor TOrderBadDtor.Destroy;
begin
  inherited;
  FValue := 0;
end;

{ inherited in middle of constructor — should warn }
constructor TOrderBadCtorMiddle.Create;
begin
  FValue := 1;
  inherited Create;
  FValue := 2;
end;

{ inherited correctly last — no warn }
destructor TOrderBadCtorMiddle.Destroy;
begin
  FValue := 0;
  inherited;
end;

end.
