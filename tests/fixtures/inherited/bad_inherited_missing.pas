unit BadInheritedMissing;

interface

type
  TMissingCtor = class
  public
    constructor Create;
    destructor Destroy; override;
  end;

  TMissingDtor = class
  public
    constructor Create;
    destructor Destroy; override;
  end;

  TMissingBoth = class
  public
    constructor Create;
    destructor Destroy; override;
  end;

implementation

{ constructor missing inherited — should warn }
constructor TMissingCtor.Create;
begin
  FValue := 1;
end;

{ destructor has inherited — no warn }
destructor TMissingCtor.Destroy;
begin
  FValue := 0;
  inherited;
end;

{ constructor has inherited — no warn }
constructor TMissingDtor.Create;
begin
  inherited;
  FValue := 1;
end;

{ destructor missing inherited — should warn }
destructor TMissingDtor.Destroy;
begin
  FValue := 0;
end;

{ both missing — should warn for both }
constructor TMissingBoth.Create;
begin
  FValue := 1;
end;

destructor TMissingBoth.Destroy;
begin
  FValue := 0;
end;

end.
