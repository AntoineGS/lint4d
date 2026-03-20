unit GoodFieldOwned;

interface

type
  TOwned = class
  private
    FButton: TObject;
  public
    constructor Create;
    destructor Destroy; override;
  end;

implementation

constructor TOwned.Create;
begin
  inherited Create;
  FButton := TObject.Create(Self);
end;

destructor TOwned.Destroy;
begin
  inherited;
end;

end.
