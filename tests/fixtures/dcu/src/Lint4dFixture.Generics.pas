unit Lint4dFixture.Generics;

interface

uses
  Generics.Collections;

type
  IGenericInterface<T> = interface
    function GetItem: T;
    procedure SetItem(const AValue: T);
  end;

  TGenericList<T> = class
  private
    FItems: TList<T>;
    FCount: Integer;
  public
    constructor Create;
    destructor Destroy; override;
    procedure Add(const AItem: T);
    function Get(AIndex: Integer): T;
    property Count: Integer read FCount;
  end;

  TGenericPair<TKey, TValue> = class
  private
    FKey: TKey;
    FValue: TValue;
  public
    constructor Create(const AKey: TKey; const AValue: TValue);
    property Key: TKey read FKey;
    property Value: TValue read FValue;
  end;

implementation

{ TGenericList<T> }

constructor TGenericList<T>.Create;
begin
  inherited Create;
  FItems := TList<T>.Create;
  FCount := 0;
end;

destructor TGenericList<T>.Destroy;
begin
  FItems.Free;
  inherited;
end;

procedure TGenericList<T>.Add(const AItem: T);
begin
  FItems.Add(AItem);
  Inc(FCount);
end;

function TGenericList<T>.Get(AIndex: Integer): T;
begin
  Result := FItems[AIndex];
end;

{ TGenericPair<TKey, TValue> }

constructor TGenericPair<TKey, TValue>.Create(const AKey: TKey; const AValue: TValue);
begin
  inherited Create;
  FKey := AKey;
  FValue := AValue;
end;

end.
