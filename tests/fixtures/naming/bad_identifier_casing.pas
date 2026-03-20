unit BadIdentifierCasing;

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
  fvalue := 10;
  localobj := nil;
end;

procedure TMyClass.DoWork;
var
  Counter: Integer;
begin
  counter := my_const;
  FValue := Counter;
end;

end.
