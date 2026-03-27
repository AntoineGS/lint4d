unit BadFieldReassign;

interface

type
  TLeaky = class
  private
    FChild: TObject;
  public
    constructor Create;
    destructor Destroy; override;
    procedure Reset;
  end;

implementation

constructor TLeaky.Create;
begin
  inherited Create;
  FChild := TObject.Create;
end;

destructor TLeaky.Destroy;
begin
  FChild.Free;
  inherited;
end;

procedure TLeaky.Reset;
begin
  FChild := TObject.Create;
end;

end.
