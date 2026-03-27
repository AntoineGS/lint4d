unit GoodFieldCreatedInMethodFreed;

interface

type
  TMyClass = class
  private
    FObj: TObject;
  public
    procedure Initialize;
    destructor Destroy; override;
  end;

implementation

procedure TMyClass.Initialize;
begin
  FObj := TObject.Create;
end;

destructor TMyClass.Destroy;
begin
  FObj.Free;
  inherited;
end;

end.
