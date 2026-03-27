unit GoodInherited;

interface

type
  TGoodBasic = class
  public
    constructor Create;
    destructor Destroy; override;
  end;

  TGoodNamed = class
  public
    constructor Create(AValue: Integer);
    destructor Destroy; override;
  end;

  TGoodSingleLine = class
  public
    constructor Create;
    destructor Destroy; override;
  end;

implementation

{ inherited first in constructor — correct }
constructor TGoodBasic.Create;
begin
  inherited;
  FValue := 1;
end;

{ inherited last in destructor — correct }
destructor TGoodBasic.Destroy;
begin
  FValue := 0;
  inherited;
end;

{ inherited Create (named form) first — correct }
constructor TGoodNamed.Create(AValue: Integer);
begin
  inherited Create;
  FValue := AValue;
end;

destructor TGoodNamed.Destroy;
begin
  FValue := 0;
  inherited;
end;

{ single statement: inherited is both first and last — correct }
constructor TGoodSingleLine.Create;
begin
  inherited;
end;

destructor TGoodSingleLine.Destroy;
begin
  inherited;
end;

end.
