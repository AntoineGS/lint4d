unit BadReassignInBranchAfterSequential;

interface

type
  TMyClass = class
  private
    FObj: TObject;
  public
    constructor Create;
    destructor Destroy; override;
    procedure DoStuff;
  end;

implementation

constructor TMyClass.Create;
begin
  inherited Create;
  FObj := TObject.Create;
end;

destructor TMyClass.Destroy;
begin
  FObj.Free;
  inherited;
end;

procedure TMyClass.DoStuff;
begin
  FObj := TObject.Create;
  if True then
    FObj := TObject.Create;
end;

end.
