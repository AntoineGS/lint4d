unit GoodFieldReassignWithFree;

interface

type
  TProper = class
  private
    FChild: TObject;
  public
    constructor Create;
    destructor Destroy; override;
    procedure Reset;
  end;

implementation

constructor TProper.Create;
begin
  inherited Create;
  FChild := TObject.Create;
end;

destructor TProper.Destroy;
begin
  FChild.Free;
  inherited;
end;

procedure TProper.Reset;
begin
  FChild.Free;
  FChild := TObject.Create;
end;

end.
