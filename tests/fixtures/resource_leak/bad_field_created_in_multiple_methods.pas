unit BadFieldCreatedInMultipleMethods;

interface

type
  TMyClass = class
  private
    FObj: TObject;
  public
    constructor Create;
    procedure Reset;
    destructor Destroy; override;
  end;

implementation

constructor TMyClass.Create;
begin
  inherited Create;
  FObj := TObject.Create;
end;

procedure TMyClass.Reset;
begin
  FObj.Free;
  FObj := TObject.Create;
end;

destructor TMyClass.Destroy;
begin
  inherited;
end;

end.
