unit GoodFieldInMethod;

interface

type
  TMyClass = class
  private
    FConnection: TFDConnection;
  public
    procedure Initialize;
    destructor Destroy; override;
  end;

implementation

procedure TMyClass.Initialize;
begin
  FConnection := TFDConnection.Create(nil);
end;

destructor TMyClass.Destroy;
begin
  FConnection.Free;
  inherited;
end;

end.
