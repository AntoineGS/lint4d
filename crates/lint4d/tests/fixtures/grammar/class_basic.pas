unit ClassBasic;

interface

type
  TMyClass = class(TObject)
  private
    FValue: Integer;
  public
    constructor Create(AValue: Integer);
    destructor Destroy; override;
    procedure DoWork;
    function GetValue: Integer;
    property Value: Integer read FValue write FValue;
  end;

implementation

constructor TMyClass.Create(AValue: Integer);
begin
  inherited Create;
  FValue := AValue;
end;

destructor TMyClass.Destroy;
begin
  inherited;
end;

procedure TMyClass.DoWork;
begin
  FValue := FValue + 1;
end;

function TMyClass.GetValue: Integer;
begin
  Result := FValue;
end;

end.
