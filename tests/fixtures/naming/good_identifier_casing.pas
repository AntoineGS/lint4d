unit GoodIdentifierCasing;

interface

type
  TMyClass = class
  private
    FValue: Integer;
  public
    constructor Create;
    procedure DoWork;
  end;

const
  MY_CONST = 42;

implementation

constructor TMyClass.Create;
var
  LocalObj: TObject;
begin
  inherited Create;
  FValue := 10;
  LocalObj := nil;
end;

procedure TMyClass.DoWork;
var
  Counter: Integer;
begin
  Counter := MY_CONST;
  FValue := Counter;
end;

end.
