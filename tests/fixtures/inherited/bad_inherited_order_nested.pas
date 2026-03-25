unit BadInheritedOrderNested;

interface

type
  TCtorInheritedInIf = class
  public
    constructor Create(AParam: Integer);
    destructor Destroy; override;
  end;

  TCtorInheritedInTry = class
  public
    constructor Create;
    destructor Destroy; override;
  end;

implementation

{ inherited inside if branch — not a direct first statement, should warn inherited-order }
constructor TCtorInheritedInIf.Create(AParam: Integer);
begin
  if AParam > 0 then
    inherited Create
  else
    inherited Create;
  FValue := AParam;
end;

destructor TCtorInheritedInIf.Destroy;
begin
  FValue := 0;
  inherited;
end;

{ inherited inside try..finally — not a direct first statement, should warn inherited-order }
constructor TCtorInheritedInTry.Create;
begin
  try
    inherited;
  except
    ;
  end;
  FValue := 1;
end;

destructor TCtorInheritedInTry.Destroy;
begin
  FValue := 0;
  inherited;
end;

end.
