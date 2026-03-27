unit BadFieldNotFreed;

interface

type
  TLeaky = class
  private
    FChild: TObject;
    FLogger: TObject;
  public
    constructor Create;
    destructor Destroy; override;
  end;

implementation

constructor TLeaky.Create;
begin
  inherited Create;
  FChild := TObject.Create;
  FLogger := TObject.Create;
end;

destructor TLeaky.Destroy;
begin
  FChild.Free;
  inherited;
end;

end.
