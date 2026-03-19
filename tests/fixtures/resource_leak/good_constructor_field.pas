unit GoodConstructorField;

interface

type
  TMyServer = class
  private
    FDatabase: TDatabase;
    FAdapter: TAdapter;
    FCache: TCache;
  public
    constructor Create;
    destructor Destroy; override;
  end;

implementation

constructor TMyServer.Create;
begin
  inherited Create;
  FDatabase := TDatabase.Create;
  FAdapter := TAdapter.Create;
  FCache := TCache.Create;
end;

destructor TMyServer.Destroy;
begin
  FCache.Free;
  FAdapter.Free;
  FDatabase.Free;
  inherited;
end;

end.
