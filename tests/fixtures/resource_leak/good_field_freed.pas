unit GoodFieldFreed;

interface

type
  TProper = class
  private
    FChild: TObject;
    FLogger: TObject;
  public
    constructor Create;
    destructor Destroy; override;
  end;

implementation

constructor TProper.Create;
begin
  inherited Create;
  FChild := TObject.Create;
  FLogger := TObject.Create;
end;

destructor TProper.Destroy;
begin
  FLogger.Free;
  FChild.Free;
  inherited;
end;

end.
